//! Аудиослой: WASAPI (Core Audio) — перечисление и события аудиосессий,
//! управление громкостью приложений.
//!
//! Вся работа с COM сосредоточена в одном потоке (`spawn` → `engine::run`).
//! COM-колбэки приходят из системных потоков и **только** кладут простые
//! данные (строки/числа) в канал — COM-объекты через поток не передаются.
//! Правила ([`crate::policy`]) применяются в том же потоке, поэтому реакция
//! на появление новой сессии мгновенная, а UI только наблюдает и шлёт команды.

pub mod binding;
pub mod process;
pub mod sessions;

use std::collections::HashSet;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use eframe::egui;
use windows::core::GUID;
use windows::Win32::Foundation::E_ACCESSDENIED;
use windows::Win32::Media::Audio::{
    AudioSessionState, AudioSessionStateActive, IAudioSessionManager2, IMMDeviceEnumerator,
    MMDeviceEnumerator,
};
use windows::Win32::System::Com::{
    CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};

use crate::config::{save_config, save_known, AppPaths, Config};
use crate::policy::loudness::{fader_gain_db, DuckAction, Limiter, LoudnessConfig, SILENCE_FLOOR_DBFS};
use crate::policy::{AppStatus, PolicyAction, PolicyInput, RuleEngine};

/// Сколько секунд длится калибровка комфортного уровня.
const CALIBRATION_SECS: u64 = 5;

/// Ключ аудиосессии (session instance identifier).
pub type SessionKey = String;

/// GUID-контекст наших изменений громкости: отличаем собственные установки
/// от изменений, сделанных пользователем или самим приложением.
pub const OUR_EVENT_CONTEXT: GUID = GUID::from_u128(0x7d3e_8c60_5a1b_4f2e_9c8d_1122_3344_55aa);

/// Как часто просыпается аудиопоток (тик политики + реакция на команды).
const LOOP_TICK: Duration = Duration::from_millis(100);

/// Статус известности приложения для UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownState {
    Trusted,
    Defaulted,
    Unidentified,
    System,
}

impl From<AppStatus> for KnownState {
    fn from(s: AppStatus) -> Self {
        match s {
            AppStatus::Trusted => KnownState::Trusted,
            AppStatus::Defaulted => KnownState::Defaulted,
            AppStatus::Unidentified => KnownState::Unidentified,
            AppStatus::System => KnownState::System,
        }
    }
}

/// Снимок сессии для UI.
#[derive(Debug, Clone)]
pub struct SessionView {
    pub key: SessionKey,
    pub pid: u32,
    /// Полный путь к exe (нижний регистр), если процесс опознан.
    pub exe_path: Option<String>,
    pub display_name: String,
    pub volume: f32,
    pub mute: bool,
    pub is_system: bool,
    pub known: KnownState,
    /// false — процесс с повышенными правами, громкость менять нельзя.
    pub controllable: bool,
    /// Сессия активна (играет звук прямо сейчас или недавно играла).
    pub active: bool,
    /// Сглаженный уровень сессии в dBFS (если есть метр и лимитер включён).
    pub level_dbfs: Option<f32>,
    /// Сессия приглушена лимитером прямо сейчас.
    pub ducked: bool,
}

/// Запись базы известных приложений для UI.
#[derive(Debug, Clone)]
pub struct KnownAppView {
    pub exe_path: String,
    pub display_name: String,
    pub trusted: bool,
    pub default_applied: bool,
    /// Приложение играет звук прямо сейчас.
    pub running: bool,
}

/// События аудиопотока для UI.
#[derive(Debug)]
pub enum AudioEvent {
    /// Сессия добавлена или её состояние изменилось.
    SessionUpserted(Box<SessionView>),
    SessionRemoved(SessionKey),
    /// Полный список известных приложений.
    KnownAppsChanged(Vec<KnownAppView>),
    /// Устройство вывода по умолчанию сменилось (или привязка выполнена).
    DeviceChanged(String),
    /// Эхо актуального конфига (после применения в аудиопотоке).
    ConfigSynced(Box<Config>),
    /// Калибровка завершена: Some(опора в dBFS) или None — не было звука.
    CalibrationDone(Option<f32>),
    /// Аудиопоток аварийно завершился.
    Fatal(String),
}

/// Команды UI → аудиопоток.
#[derive(Debug, Clone)]
pub enum Command {
    SetVolume { key: SessionKey, volume: f32 },
    SetMute { key: SessionKey, mute: bool },
    TrustApp { exe_path: String, trusted: bool },
    ForgetApp { exe_path: String },
    UpdateConfig(Box<Config>),
    /// Запустить калибровку комфортного уровня (5 секунд слушайте контент).
    CalibrateLoudness,
    Shutdown,
}

/// Внутренние события COM-колбэков → аудиопоток (только простые данные).
#[derive(Debug)]
pub(crate) enum RawEvent {
    /// Создана новая аудиосессия (нужен пересканирование).
    Rescan,
    StateChanged { key: SessionKey, state: AudioSessionState },
    VolumeChanged { key: SessionKey, volume: f32, mute: bool, by_user: bool },
    Disconnected { key: SessionKey },
}

/// Запустить аудиопоток. Возвращает каналы команд и событий.
pub fn spawn(ui_ctx: egui::Context, config: Config) -> anyhow::Result<(Sender<Command>, Receiver<AudioEvent>)> {
    let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();
    let (ev_tx, ev_rx) = mpsc::channel::<AudioEvent>();
    thread::Builder::new()
        .name("miss-audio".into())
        .spawn(move || {
            match engine::run(ui_ctx, cmd_rx, ev_tx.clone(), config) {
                Ok(()) => tracing::info!("аудиопоток остановлен"),
                Err(e) => {
                    tracing::error!("аудиопоток упал: {e:#}");
                    let _ = ev_tx.send(AudioEvent::Fatal(format!("{e:#}")));
                }
            }
        })
        .context("запуск аудиопотока")?;
    Ok((cmd_tx, ev_rx))
}

/// PWSTR из COM-вызова (CoTaskMemFree обязателен) → String.
pub(crate) fn pwstr_to_string(ptr: windows::core::PWSTR) -> windows::core::Result<String> {
    let s = unsafe { ptr.to_string() };
    unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(ptr.as_ptr().cast())) };
    s.map_err(|e| {
        windows::core::Error::new(windows::core::HRESULT(0x8007_000D_u32 as i32), e.to_string())
    })
}

mod engine {
    use super::*;

    struct CoGuard;
    impl Drop for CoGuard {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }

    pub(super) fn run(
        ui_ctx: egui::Context,
        cmd_rx: Receiver<Command>,
        ev_tx: Sender<AudioEvent>,
        config: Config,
    ) -> anyhow::Result<()> {
        // MTA: WASAPI-объекты свободно потоки (free-threaded).
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        hr.ok().context("CoInitializeEx(MTA) не удался")?;
        let _com = CoGuard;

        let paths = crate::config::paths()?;
        let known = crate::config::load_known(&paths.known);
        // Первый запуск: базы known-apps.toml ещё нет. Знакомимся с уже
        // запущенными приложениями, НЕ приглушая их, — глушим только тех,
        // кто появится после установки утилиты.
        let first_run = !paths.known.exists();
        let enumerator: IMMDeviceEnumerator = unsafe {
            windows::Win32::System::Com::CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
        }
        .context("создание IMMDeviceEnumerator")?;

        let (raw_tx, raw_rx) = mpsc::channel::<RawEvent>();

        let mut engine = Engine {
            ui_ctx,
            ev_tx,
            paths,
            policy: RuleEngine::new(config.clone(), known),
            config,
            enumerator: enumerator.clone(),
            session_notifier: sessions::create_session_notifier(raw_tx.clone()),
            binding: None,
            registry: sessions::SessionRegistry::default(),
            raw_tx,
            tick_count: 0,
            seeding: first_run,
            known_touched: false,
            limiter: Limiter::default(),
            calibration: None,
            level_emit_counter: 0,
            last_device_gain_db: 0.0,
        };

        engine.bind_default_device()?;
        if first_run {
            engine.finish_seeding();
        }
        engine.emit_all();

        loop {
            let now = Instant::now();
            match cmd_rx.recv_timeout(LOOP_TICK) {
                Ok(Command::Shutdown) => break,
                Ok(cmd) => engine.on_command(cmd, now),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            // Вытягиваем события COM-колбэков.
            while let Ok(raw) = raw_rx.try_recv() {
                engine.on_raw(raw, Instant::now());
            }
            // Тик политики: отложенные re-assert и т.п.
            for action in engine.policy.handle(PolicyInput::Tick(Instant::now()), Instant::now()) {
                engine.apply(action, Instant::now());
            }
            // Тик лимитера громкости (v2): метры, калибровка, приглушение.
            engine.loudness_tick(Instant::now());
            // Устройство по умолчанию отслеживаем опросом (~1.5 c): в windows-rs 0.62
            // нет RegisterNotificationClient, а опрос id дешевле и надёжнее.
            engine.tick_count = engine.tick_count.wrapping_add(1);
            if engine.tick_count % 15 == 0 {
                engine.check_default_device();
            }
        }

        engine.cleanup();
        Ok(())
    }

    struct Engine {
        ui_ctx: egui::Context,
        ev_tx: Sender<AudioEvent>,
        paths: AppPaths,
        config: Config,
        enumerator: IMMDeviceEnumerator,
        /// COM-объект «новая сессия создана» (живёт всё время работы).
        session_notifier: windows::Win32::Media::Audio::IAudioSessionNotification,
        binding: Option<binding::DeviceBinding>,
        registry: sessions::SessionRegistry,
        policy: RuleEngine,
        raw_tx: Sender<RawEvent>,
        tick_count: u32,
        /// Режим первого запуска: сессии запоминаются без применения дефолта.
        seeding: bool,
        /// Появились ли новые записи в базе во время seeding.
        known_touched: bool,
        /// Лимитер громкости (v2).
        limiter: Limiter,
        /// Идёт калибровка: (дедлайн, максимальный уровень за окно).
        calibration: Option<(Instant, f32)>,
        /// Счётчик тиков для периодической рассылки уровней в UI.
        level_emit_counter: u32,
        /// Последний известный вклад мастер-громкости (для UI).
        last_device_gain_db: f32,
    }

    impl Engine {
        fn send(&self, event: AudioEvent) {
            let _ = self.ev_tx.send(event);
            self.ui_ctx.request_repaint();
        }

        /// Завершить режим первого запуска: сохранить запомненные приложения.
    fn finish_seeding(&mut self) {
        self.seeding = false;
        if self.known_touched {
            self.known_touched = false;
            let known = self.policy.known().clone();
            if let Err(e) = save_known(&self.paths.known, &known) {
                tracing::error!("сохранение known-apps.toml: {e:#}");
            }
            tracing::info!(
                "первый запуск: уже запущенные приложения запомнены без приглушения ({})",
                known.len()
            );
        }
    }

        /// Тик лимитера громкости: опрос метров, калибровка, приглушение.
        ///
        /// Метр сессии измеряет пик потока **до фейдера**, поэтому слышимый
        /// уровень = метр + фейдер сессии + мастер-фейдер устройства. Для
        /// стабильности решения лимитер использует фейдер **базы** (а не
        /// текущего прижатого значения) — иначе получилось бы самовозбуждение.
        fn loudness_tick(&mut self, now: Instant) {
            // Мастер-громкость устройства; None — устройство выключено/заглушено.
            let device_gain_db = self.device_gain_db();
            self.last_device_gain_db = device_gain_db.unwrap_or(-120.0);

            // Сглаженные метры сессий (сырые, без фейдеров).
            let keys: Vec<SessionKey> = self.registry.keys().cloned().collect();
            let mut levels: Vec<(SessionKey, f32)> = Vec::new();
            for key in &keys {
                let Some(entry) = self.registry.get_mut(key) else { continue };
                if entry.core.is_system || !entry.controllable || entry.volume_state.1 {
                    continue; // системные / недоступные / заглушённые сессии
                }
                let Some(meter) = &entry.meter else { continue };
                // SAFETY: COM-объект принадлежит этому потоку (MTA).
                let peak = unsafe { meter.GetPeakValue() }.unwrap_or(0.0);
                let level = entry.smoother.update(peak);
                levels.push((key.clone(), level));
            }

            // Калибровка комфортного уровня: максимум слышимого уровня за окно.
            // Работает и при выключенном автоприглушении — опора сохранится.
            if let Some((deadline, max_so_far)) = self.calibration {
                let mut max = max_so_far;
                for (key, meter_level) in &levels {
                    let current = self.current_volume(key);
                    let audible =
                        *meter_level + fader_gain_db(current) + device_gain_db.unwrap_or(-120.0);
                    if audible > max {
                        max = audible;
                    }
                }
                if now >= deadline {
                    self.calibration = None;
                    if max <= SILENCE_FLOOR_DBFS {
                        tracing::info!("калибровка не удалась: тишина");
                        self.send(AudioEvent::CalibrationDone(None));
                    } else {
                        let reference = max - 1.0;
                        tracing::info!("калибровка: опора {reference:.1} dBFS");
                        self.config.loudness_reference_dbfs = Some(reference);
                        if let Err(e) = save_config(&self.paths.config, &self.config) {
                            tracing::error!("сохранение конфига: {e:#}");
                        }
                        self.limiter.clear();
                        self.send(AudioEvent::CalibrationDone(Some(reference)));
                        self.send(AudioEvent::ConfigSynced(Box::new(self.config.clone())));
                    }
                } else {
                    self.calibration = Some((deadline, max));
                }
            }

            // Лимитер: работает только при включённой опции и известной опоре.
            self.level_emit_counter = self.level_emit_counter.wrapping_add(1);
            let ui_refresh = self.level_emit_counter % 5 == 0; // ~2 Гц для полосок уровня
            if !self.config.loudness_enabled {
                return;
            }
            let Some(reference) = self.config.loudness_reference_dbfs else {
                return;
            };
            let Some(device_gain_db) = device_gain_db else {
                return; // устройство заглушено — слышимого звука нет
            };
            let cfg = LoudnessConfig {
                ceiling_offset_db: self.config.loudness_ceiling_offset_db,
                max_duck_db: 12.0,
            };
            for (key, meter_level) in levels {
                // Слышимый уровень относительно базовой громкости сессии.
                let base = self
                    .limiter
                    .base_of(&key)
                    .unwrap_or_else(|| self.current_volume(&key));
                let audible = meter_level + fader_gain_db(base) + device_gain_db;
                let current = self.current_volume(&key);
                match self.limiter.update(&key, audible, current, &cfg, reference) {
                    DuckAction::None => {
                        if ui_refresh {
                            self.emit_view(&key); // обновить полоску уровня
                        }
                    }
                    DuckAction::Set { volume, ducking } => {
                        let name = self
                            .registry
                            .info_of(&key)
                            .map(|i| i.display_name)
                            .unwrap_or_else(|| key.clone());
                        if ducking {
                            tracing::info!(
                                "приглушение «{name}»: {:.0} dBFS → громкость {:.0}%",
                                audible,
                                volume * 100.0
                            );
                        } else {
                            tracing::info!(
                                "приглушение снято «{name}»: {:.0} dBFS → громкость {:.0}%",
                                audible,
                                volume * 100.0
                            );
                        }
                        self.set_volume(&key, volume);
                        if let Some(entry) = self.registry.get_mut(&key) {
                            entry.ducked = ducking;
                        }
                        self.emit_view(&key);
                    }
                }
            }
        }

        /// Текущая громкость сессии (0..1).
        fn current_volume(&self, key: &SessionKey) -> f32 {
            self.registry
                .get(key)
                .map(|e| e.volume_state.0)
                .unwrap_or(0.0)
        }

        /// Мастер-громкость устройства в дБ; None — заглушено или недоступно.
        fn device_gain_db(&self) -> Option<f32> {
            let binding = self.binding.as_ref()?;
            // SAFETY: COM-объект принадлежит этому потоку (MTA).
            unsafe {
                let muted = binding.endpoint_volume.GetMute().ok()?.as_bool();
                if muted {
                    return None;
                }
                let volume = binding.endpoint_volume.GetMasterVolumeLevelScalar().ok()?;
                Some(fader_gain_db(volume))
            }
        }

        /// Откатить все приглушения (при выключении лимитера).
        fn restore_ducked(&mut self) {
            for (key, base) in self.limiter.ducked() {
                let name = self
                    .registry
                    .info_of(&key)
                    .map(|i| i.display_name)
                    .unwrap_or_else(|| key.clone());
                tracing::info!("лимитер выключен: «{name}» → {:.0}%", base * 100.0);
                if let Some(entry) = self.registry.get_mut(&key) {
                    entry.ducked = false;
                }
                self.set_volume(&key, base);
                self.emit_view(&key);
            }
            self.limiter.clear();
        }

        /// Опрос устройства по умолчанию: если id сменился — перепривязка.
        fn check_default_device(&mut self) {
            let new_id = match binding::default_device(&self.enumerator) {
                Ok((_, id, _, _)) => id,
                Err(e) => {
                    tracing::debug!("опрос устройства по умолчанию: {e:#}");
                    return;
                }
            };
            let changed = self
                .binding
                .as_ref()
                .map(|b| b.device_id != new_id)
                .unwrap_or(true);
            if changed {
                tracing::info!("устройство по умолчанию сменилось, перепривязываюсь");
                if let Err(e) = self.bind_default_device() {
                    tracing::error!("перепривязка устройства не удалась: {e:#}");
                }
            }
        }

        /// Привязка к устройству вывода по умолчанию + переподписка на сессии.
        fn bind_default_device(&mut self) -> anyhow::Result<()> {
            let (device, device_id, device_name, endpoint_volume) =
                binding::default_device(&self.enumerator).context("устройство по умолчанию")?;
            let manager: IAudioSessionManager2 =
                binding::activate_session_manager(&device).context("активация IAudioSessionManager2")?;

            if let Some(old) = self.binding.replace(binding::DeviceBinding {
                device_id: device_id.clone(),
                device_name: device_name.clone(),
                manager: manager.clone(),
                endpoint_volume,
            }) {
                unsafe {
                    let _ = old.manager.UnregisterSessionNotification(&self.session_notifier);
                }
            }
            unsafe {
                manager
                    .RegisterSessionNotification(&self.session_notifier)
                    .context("RegisterSessionNotification")?;
            }

            for key in self.registry.clear() {
                self.limiter.remove(&key);
                for action in self.policy.handle(PolicyInput::SessionGone(key.clone()), Instant::now()) {
                    self.apply(action, Instant::now());
                }
                self.send(AudioEvent::SessionRemoved(key));
            }
            self.policy.reset_pending();

            self.rescan(Instant::now());
            tracing::info!(
                "привязан к устройству «{}» ({}), активных сессий: {}",
                device_name,
                device_id,
                self.registry.keys().count()
            );
            self.send(AudioEvent::DeviceChanged(device_name));
            Ok(())
        }

        /// Перечитать все сессии текущего устройства; новые — в политику.
        fn rescan(&mut self, now: Instant) {
            let Some(binding) = self.binding.clone() else { return };
            let session_enum = match unsafe { binding.manager.GetSessionEnumerator() } {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!("GetSessionEnumerator: {e}");
                    return;
                }
            };
            let count = match unsafe { session_enum.GetCount() } {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("GetCount: {e}");
                    return;
                }
            };

            let mut seen = HashSet::new();
            for i in 0..count {
                let control = match unsafe { session_enum.GetSession(i) } {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let (key, is_new) =
                    match self.registry.ensure_entry(control, &self.raw_tx) {
                        Ok(pair) => pair,
                        Err(e) => {
                            tracing::debug!("сессия #{i} пропущена: {e}");
                            continue;
                        }
                    };
                seen.insert(key.clone());
                if is_new {
                    if let Some(info) = self.registry.info_of(&key) {
                        tracing::debug!(
                            "новая сессия: {} (pid={}, {})",
                            info.display_name,
                            info.pid,
                            info.exe_path.as_deref().unwrap_or("<не опознан>")
                        );
                        if self.seeding {
                            // Первый запуск: только запоминаем приложение.
                            if let Some(path) = info.exe_path.clone() {
                                self.policy
                                    .known_mut()
                                    .entry(path)
                                    .or_insert_with(|| crate::config::KnownApp {
                                        trusted: false,
                                        default_applied: true,
                                        display_name: info.display_name.clone(),
                                    });
                                self.known_touched = true;
                            }
                        } else {
                            for action in
                                self.policy.handle(PolicyInput::SessionAppeared(info), now)
                            {
                                self.apply(action, now);
                            }
                        }
                        self.emit_view(&key);
                    }
                }
            }

            for key in self.registry.remove_missing(&seen) {
                self.limiter.remove(&key);
                for action in self.policy.handle(PolicyInput::SessionGone(key.clone()), now) {
                    self.apply(action, now);
                }
                self.send(AudioEvent::SessionRemoved(key));
            }
        }

        fn on_raw(&mut self, raw: RawEvent, now: Instant) {
            match raw {
                RawEvent::Rescan => self.rescan(now),
                RawEvent::StateChanged { key, state } => {
                    let active = state == AudioSessionStateActive;
                    if self.registry.set_active(&key, active) {
                        self.emit_view(&key);
                    }
                }
                RawEvent::VolumeChanged { key, volume, mute, by_user } => {
                    self.registry.update_volume(&key, volume, mute);
                    if by_user {
                        // Пользователь взял громкость под контроль — новая база лимитера.
                        self.limiter.set_base(&key, volume);
                    }
                    for action in self.policy.handle(
                        PolicyInput::SessionVolumeChanged { key: key.clone(), volume, mute, by_user },
                        now,
                    ) {
                        self.apply(action, now);
                    }
                    self.emit_view(&key);
                }
                RawEvent::Disconnected { key } => {
                    if self.registry.remove(&key).is_some() {
                        self.limiter.remove(&key);
                        for action in
                            self.policy.handle(PolicyInput::SessionGone(key.clone()), now)
                        {
                            self.apply(action, now);
                        }
                        self.send(AudioEvent::SessionRemoved(key));
                    }
                }
            }
        }

        fn on_command(&mut self, cmd: Command, now: Instant) {
            match cmd {
                Command::Shutdown => {} // обрабатывается в главном цикле
                Command::SetVolume { key, volume } => self.set_volume(&key, volume),
                Command::SetMute { key, mute } => self.set_mute(&key, mute),
                Command::TrustApp { exe_path, trusted } => {
                    for action in self.policy.handle(
                        PolicyInput::TrustApp { exe_path, trusted },
                        now,
                    ) {
                        self.apply(action, now);
                    }
                    self.emit_known();
                }
                Command::ForgetApp { exe_path } => {
                    for action in self.policy.handle(PolicyInput::ForgetApp { exe_path }, now) {
                        self.apply(action, now);
                    }
                    self.emit_known();
                }
                Command::UpdateConfig(new_config) => {
                    let mut new_config = *new_config;
                    tracing::info!(
                        "UpdateConfig: приглушение {} → {}, опора {:?}",
                        self.config.loudness_enabled,
                        new_config.loudness_enabled,
                        new_config.loudness_reference_dbfs
                    );
                    if new_config.autostart != self.config.autostart {
                        if let Err(e) = crate::autostart::set_enabled(new_config.autostart) {
                            tracing::error!("автозапуск не переключён: {e:#}");
                            new_config.autostart = self.config.autostart;
                        }
                    }
                    // Выключение лимитера: снимаем все приглушения.
                    if self.config.loudness_enabled && !new_config.loudness_enabled {
                        self.restore_ducked();
                    }
                    for action in self.policy.handle(
                        PolicyInput::ConfigUpdated(Box::new(new_config.clone())),
                        now,
                    ) {
                        self.apply(action, now);
                    }
                    self.config = new_config.clone();
                    if let Err(e) = save_config(&self.paths.config, &self.config) {
                        tracing::error!("сохранение конфига: {e:#}");
                    }
                    self.send(AudioEvent::ConfigSynced(Box::new(new_config)));
                }
                Command::CalibrateLoudness => {
                    tracing::info!("калибровка комфортного уровня: слушайте контент {CALIBRATION_SECS} с");
                    self.calibration =
                        Some((now + std::time::Duration::from_secs(CALIBRATION_SECS), -100.0));
                }
            }
        }

        fn apply(&mut self, action: PolicyAction, _now: Instant) {
            match action {
                PolicyAction::SetVolume { key, volume, reason } => {
                    let name = self
                        .registry
                        .info_of(&key)
                        .map(|i| i.display_name)
                        .unwrap_or_else(|| key.clone());
                    tracing::info!("громкость «{name}» → {:.0}% ({reason})", volume * 100.0);
                    self.set_volume(&key, volume);
                    // Правило дефолта задаёт «базовую» громкость — лимитер
                    // прижимает относительно неё.
                    self.limiter.set_base(&key, volume);
                }
                PolicyAction::SaveKnown => {
                    let known = self.policy.known().clone();
                    if let Err(e) = save_known(&self.paths.known, &known) {
                        tracing::error!("сохранение known-apps.toml: {e:#}");
                    }
                    self.emit_known();
                }
            }
        }

        fn set_volume(&mut self, key: &SessionKey, volume: f32) {
            let v = volume.clamp(0.0, 1.0);
            let Some(entry) = self.registry.get_mut(key) else { return };
            // SAFETY: вызов в аудиопотоке, который владеет COM-объектами (MTA).
            let result = unsafe { entry.volume.SetMasterVolume(v, &OUR_EVENT_CONTEXT) };
            match result {
                Ok(()) => entry.volume_state.0 = v,
                Err(e) if e.code() == E_ACCESSDENIED => {
                    entry.controllable = false;
                    tracing::warn!(
                        "сессия {key} недоступна: процесс с повышенными правами"
                    );
                }
                Err(e) => tracing::warn!("SetMasterVolume({key}): {e}"),
            }
            self.emit_view(key);
        }

        fn set_mute(&mut self, key: &SessionKey, mute: bool) {
            let Some(entry) = self.registry.get_mut(key) else { return };
            let result = unsafe { entry.volume.SetMute(mute, &OUR_EVENT_CONTEXT) };
            match result {
                Ok(()) => entry.volume_state.1 = mute,
                Err(e) if e.code() == E_ACCESSDENIED => entry.controllable = false,
                Err(e) => tracing::warn!("SetMute({key}): {e}"),
            }
            self.emit_view(key);
        }

        fn emit_view(&self, key: &SessionKey) {
            if let Some(view) = self.view_of(key) {
                self.send(AudioEvent::SessionUpserted(Box::new(view)));
            }
        }

        fn view_of(&self, key: &SessionKey) -> Option<SessionView> {
            let entry = self.registry.get(key)?;
            let limiter_active =
                self.config.loudness_enabled && self.config.loudness_reference_dbfs.is_some();
            // Слышимый уровень для полоски: метр + текущие фейдеры.
            let level_dbfs = if limiter_active {
                entry.smoother.current().map(|meter| {
                    meter
                        + fader_gain_db(entry.volume_state.0)
                        + self.last_device_gain_db
                })
            } else {
                None
            };
            Some(SessionView {
                key: entry.key.clone(),
                pid: entry.core.pid,
                exe_path: entry.core.exe_path.clone(),
                display_name: entry.core.display_name.clone(),
                volume: entry.volume_state.0,
                mute: entry.volume_state.1,
                is_system: entry.core.is_system,
                known: KnownState::from(
                    self.policy
                        .status_for(entry.core.exe_path.as_deref(), entry.core.is_system),
                ),
                controllable: entry.controllable,
                active: entry.active,
                level_dbfs,
                ducked: limiter_active && entry.ducked,
            })
        }

        fn emit_all(&mut self) {
            let keys: Vec<SessionKey> = self.registry.keys().cloned().collect();
            for key in keys {
                self.emit_view(&key);
            }
            self.emit_known();
            let device = self
                .binding
                .as_ref()
                .map(|b| b.device_name.clone())
                .unwrap_or_default();
            self.send(AudioEvent::DeviceChanged(device));
            self.send(AudioEvent::ConfigSynced(Box::new(self.config.clone())));
        }

        fn emit_known(&self) {
            let running = self.registry.running_paths();
            let views: Vec<KnownAppView> = self
                .policy
                .known()
                .iter()
                .map(|(path, app)| KnownAppView {
                    exe_path: path.clone(),
                    display_name: app.display_name.clone(),
                    trusted: app.trusted,
                    default_applied: app.default_applied,
                    running: running.contains(path),
                })
                .collect();
            self.send(AudioEvent::KnownAppsChanged(views));
        }

        fn cleanup(&mut self) {
            if let Some(binding) = self.binding.take() {
                unsafe {
                    let _ = binding.manager.UnregisterSessionNotification(&self.session_notifier);
                }
            }
        }
    }
}
