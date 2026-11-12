//! Реестр аудиосессий + COM-колбэки событий сессий.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::Sender;

use windows::core::{implement, Interface, PCWSTR};
use windows::Win32::Foundation::S_OK;
use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
use windows::Win32::Media::Audio::{
    AudioSessionState, AudioSessionStateActive, IAudioSessionControl, IAudioSessionControl2,
    IAudioSessionEvents, IAudioSessionEvents_Impl, IAudioSessionNotification,
    IAudioSessionNotification_Impl, ISimpleAudioVolume,
};

use super::{pwstr_to_string, RawEvent, SessionKey, OUR_EVENT_CONTEXT};
use crate::policy::display_name_of;
use crate::policy::loudness::Smoother;

/// Статические данные сессии (не меняются за время жизни).
#[derive(Debug, Clone)]
pub(crate) struct SessionCore {
    pub pid: u32,
    /// Полный путь к exe в нижнем регистре; None, если процесс не опознан.
    pub exe_path: Option<String>,
    pub display_name: String,
    pub is_system: bool,
}

/// Владеет всеми COM-объектами одной сессии.
pub(crate) struct SessionEntry {
    pub key: SessionKey,
    /// IAudioSessionControl — Register/Unregister уведомлений и GetState.
    pub control: IAudioSessionControl,
    pub volume: ISimpleAudioVolume,
    /// Объект колбэка: держим живым, пока сессия в реестре.
    pub events: IAudioSessionEvents,
    /// Метр пиков сессии (если сессия его отдаёт) — для лимитера v2.
    pub meter: Option<IAudioMeterInformation>,
    /// Сглаживатель уровня громкости сессии (dBFS).
    pub smoother: Smoother,
    /// Сессия приглушена лимитером прямо сейчас.
    pub ducked: bool,
    pub core: SessionCore,
    /// (громкость 0..1, mute) — последнее известное состояние.
    pub volume_state: (f32, bool),
    pub active: bool,
    pub controllable: bool,
}

impl Drop for SessionEntry {
    fn drop(&mut self) {
        // SAFETY: объекты ещё живы; снимаем подписку до Release интерфейсов.
        unsafe {
            let _ = self
                .control
                .UnregisterAudioSessionNotification(&self.events);
        }
    }
}

/// Все известные сессии текущего устройства.
#[derive(Default)]
pub(crate) struct SessionRegistry {
    map: HashMap<SessionKey, SessionEntry>,
}

impl SessionRegistry {
    /// Найти сессию по контролу; создать запись (и подписаться на события),
    /// если она новая. Возвращает (ключ, создана_ли_заново).
    pub fn ensure_entry(
        &mut self,
        control: IAudioSessionControl,
        tx: &Sender<RawEvent>,
    ) -> windows::core::Result<(SessionKey, bool)> {
        let control2: IAudioSessionControl2 = control.cast()?;
        let key = pwstr_to_string(unsafe { control2.GetSessionInstanceIdentifier()? })?;
        if self.map.contains_key(&key) {
            return Ok((key, false));
        }

        let pid = unsafe { control2.GetProcessId().unwrap_or(0) };
        let exe_path = super::process::process_image_path(pid).map(|p| p.to_lowercase());
        let display_name = exe_path
            .as_deref()
            .map(display_name_of)
            .unwrap_or_else(|| format!("PID {pid}"));
        // ВАЖНО: IsSystemSoundsSession возвращает S_FALSE для обычных приложений —
        // это тоже «success» для is_ok(), поэтому сравниваем строго с S_OK.
        let is_system = unsafe { control2.IsSystemSoundsSession() } == S_OK;

        let volume: ISimpleAudioVolume = control.cast()?;
        let vol = unsafe { volume.GetMasterVolume().unwrap_or(1.0).clamp(0.0, 1.0) };
        let mute = unsafe { volume.GetMute().map(|b| b.as_bool()).unwrap_or(false) };
        let active = match unsafe { control.GetState() } {
            Ok(state) => state == AudioSessionStateActive,
            Err(_) => false,
        };

        let events: IAudioSessionEvents =
            SessionEventsCb { key: key.clone(), tx: tx.clone() }.into();
        unsafe {
            control.RegisterAudioSessionNotification(&events)?;
        }

        // Метр пиков: сессии обычно отдают IAudioMeterInformation через QueryInterface.
        let meter: Option<IAudioMeterInformation> = match control.cast() {
            Ok(m) => Some(m),
            Err(_) => {
                tracing::debug!("сессия {key} без метра (нет IAudioMeterInformation)");
                None
            }
        };

        self.map.insert(
            key.clone(),
            SessionEntry {
                key: key.clone(),
                control,
                volume,
                events,
                meter,
                smoother: Smoother::new(0.5, 0.15),
                ducked: false,
                core: SessionCore { pid, exe_path, display_name, is_system },
                volume_state: (vol, mute),
                active,
                controllable: true,
            },
        );
        Ok((key, true))
    }

    pub fn get(&self, key: &SessionKey) -> Option<&SessionEntry> {
        self.map.get(key)
    }

    pub fn get_mut(&mut self, key: &SessionKey) -> Option<&mut SessionEntry> {
        self.map.get_mut(key)
    }

    pub fn info_of(&self, key: &SessionKey) -> Option<crate::policy::SessionInfo> {
        let e = self.map.get(key)?;
        Some(crate::policy::SessionInfo {
            key: e.key.clone(),
            pid: e.core.pid,
            exe_path: e.core.exe_path.clone(),
            display_name: e.core.display_name.clone(),
            is_system: e.core.is_system,
            volume: e.volume_state.0,
            mute: e.volume_state.1,
            active: e.active,
        })
    }

    /// Отметить активность; true, если состояние изменилось.
    pub fn set_active(&mut self, key: &SessionKey, active: bool) -> bool {
        match self.map.get_mut(key) {
            Some(e) if e.active != active => {
                e.active = active;
                true
            }
            _ => false,
        }
    }

    pub fn update_volume(&mut self, key: &SessionKey, volume: f32, mute: bool) {
        if let Some(e) = self.map.get_mut(key) {
            e.volume_state = (volume.clamp(0.0, 1.0), mute);
        }
    }

    /// Удалить все сессии, кроме перечисленных; возвращает ключи удалённых.
    pub fn remove_missing(&mut self, seen: &HashSet<SessionKey>) -> Vec<SessionKey> {
        let gone: Vec<SessionKey> = self
            .map
            .keys()
            .filter(|k| !seen.contains(*k))
            .cloned()
            .collect();
        for k in &gone {
            self.map.remove(k);
        }
        gone
    }

    pub fn remove(&mut self, key: &SessionKey) -> Option<SessionEntry> {
        self.map.remove(key)
    }

    /// Очистить целиком (смена устройства); возвращает ключи удалённых.
    pub fn clear(&mut self) -> Vec<SessionKey> {
        self.map.drain().map(|(k, _)| k).collect()
    }

    pub fn keys(&self) -> impl Iterator<Item = &SessionKey> {
        self.map.keys()
    }

    /// Пути exe (нижний регистр) сессий, живых прямо сейчас.
    pub fn running_paths(&self) -> HashSet<String> {
        self.map
            .values()
            .filter_map(|e| e.core.exe_path.clone())
            .collect()
    }
}

/// Колбэк событий одной сессии. Вызывается из системных потоков WASAPI,
/// поэтому только кладёт данные в канал — никаких COM-вызовов внутри.
#[implement(IAudioSessionEvents)]
pub(crate) struct SessionEventsCb {
    pub key: SessionKey,
    pub tx: Sender<RawEvent>,
}

impl IAudioSessionEvents_Impl for SessionEventsCb_Impl {
    fn OnDisplayNameChanged(
        &self,
        _new_display_name: &PCWSTR,
        _event_context: *const windows::core::GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnIconPathChanged(
        &self,
        _new_icon_path: &PCWSTR,
        _event_context: *const windows::core::GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnSimpleVolumeChanged(
        &self,
        new_volume: f32,
        new_mute: windows::core::BOOL,
        event_context: *const windows::core::GUID,
    ) -> windows::core::Result<()> {
        // Наш собственный контекст = это мы меняли громкость.
        let by_user = event_context.is_null() || unsafe { *event_context } != OUR_EVENT_CONTEXT;
        let _ = self.tx.send(RawEvent::VolumeChanged {
            key: self.key.clone(),
            volume: new_volume,
            mute: new_mute.as_bool(),
            by_user,
        });
        Ok(())
    }

    fn OnChannelVolumeChanged(
        &self,
        _channel_count: u32,
        _new_channel_volume_array: *const f32,
        _changed_channel: u32,
        _event_context: *const windows::core::GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnGroupingParamChanged(
        &self,
        _new_grouping_param: *const windows::core::GUID,
        _event_context: *const windows::core::GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnStateChanged(&self, new_state: AudioSessionState) -> windows::core::Result<()> {
        let _ = self.tx.send(RawEvent::StateChanged {
            key: self.key.clone(),
            state: new_state,
        });
        Ok(())
    }

    fn OnSessionDisconnected(
        &self,
        _disconnect_reason: windows::Win32::Media::Audio::AudioSessionDisconnectReason,
    ) -> windows::core::Result<()> {
        let _ = self.tx.send(RawEvent::Disconnected { key: self.key.clone() });
        Ok(())
    }
}

/// Колбэк «создана новая сессия» (регистрируется на IAudioSessionManager2).
#[implement(IAudioSessionNotification)]
pub(crate) struct SessionNotificationCb {
    pub tx: Sender<RawEvent>,
}

impl IAudioSessionNotification_Impl for SessionNotificationCb_Impl {
    fn OnSessionCreated(
        &self,
        _new_session: windows::core::Ref<IAudioSessionControl>,
    ) -> windows::core::Result<()> {
        // Не работаем с COM-объектом в чужом потоке: просим аудиопоток пересканировать.
        let _ = self.tx.send(RawEvent::Rescan);
        Ok(())
    }
}

/// Создать COM-объект уведомления о новых сессиях (живёт до unregister).
pub(crate) fn create_session_notifier(tx: Sender<RawEvent>) -> IAudioSessionNotification {
    SessionNotificationCb { tx }.into()
}
