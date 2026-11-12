//! Движок правил: превращает события аудиослоя в команды установки громкости.
//!
//! Чистая логика без Windows-API: тестируется на фейковых событиях (см. `tests`).
//! Выполняется в аудиопотоке — реакция на появление сессии мгновенная.

pub mod loudness;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::config::{Config, KnownApp, KnownMap};

/// Ключ аудиосессии (instance identifier, уникален для каждого запуска сессии).
pub type SessionKey = String;

/// Информация о сессии, достаточная движку для принятия решения.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub key: SessionKey,
    pub pid: u32,
    /// Полный путь к exe в нижнем регистре; `None`, если процесс не удалось опознать.
    pub exe_path: Option<String>,
    /// Имя exe для отображения.
    pub display_name: String,
    pub is_system: bool,
    pub volume: f32,
    pub mute: bool,
    pub active: bool,
}

/// Входные события движка.
#[derive(Debug, Clone)]
pub enum PolicyInput {
    /// Появилась новая сессия (создана или впервые замечена при сканировании).
    SessionAppeared(SessionInfo),
    /// Громкость/мьют сессии изменились извне (пользователь или само приложение).
    SessionVolumeChanged {
        key: SessionKey,
        volume: f32,
        mute: bool,
        /// true — изменил человек (через sndvol/микшер), false — мы сами или приложение.
        by_user: bool,
    },
    /// Сессия исчезла (процесс умер, устройство переподключено и т.д.).
    SessionGone(SessionKey),
    /// Регулярный тик таймера (для отложенных re-assert).
    Tick(Instant),
    /// Настройки обновлены.
    ConfigUpdated(Box<Config>),
    /// Приложение помечено доверенным/недоверенным из UI.
    TrustApp { exe_path: String, trusted: bool },
    /// Приложение «забыто»: дефолт применится заново, в т.ч. к уже играющим сессиям.
    ForgetApp { exe_path: String },
}

/// Действие, которое аудиослой должен выполнить.
#[derive(Debug, Clone, PartialEq)]
pub enum PolicyAction {
    SetVolume { key: SessionKey, volume: f32, reason: &'static str },
    SaveKnown,
}

/// Статус приложения для отображения в UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppStatus {
    /// Доверенное — громкость никогда не меняется автоматически.
    Trusted,
    /// Дефолт уже применён (при перезапуске не трогаем).
    Defaulted,
    /// Системные звуки Windows — не приложение.
    System,
    /// Не опознан (нет пути к exe) — политика не применяется.
    Unidentified,
}

#[derive(Clone)]
struct ReAssert {
    due: Instant,
    key: SessionKey,
    volume: f32,
}

/// Движок правил. Единственный владелец базы известных приложений в рантайме.
pub struct RuleEngine {
    config: Config,
    known: KnownMap,
    /// Активные сессии: ключ → путь exe (для Trust/Forget по пути).
    session_paths: BTreeMap<SessionKey, String>,
    reasserts: Vec<ReAssert>,
}

impl RuleEngine {
    pub fn new(config: Config, known: KnownMap) -> Self {
        Self {
            config,
            known,
            session_paths: BTreeMap::new(),
            reasserts: Vec::new(),
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn known(&self) -> &KnownMap {
        &self.known
    }

    /// Прямой доступ к базе (для первого запуска: знакомство с уже играющими).
    pub fn known_mut(&mut self) -> &mut KnownMap {
        &mut self.known
    }

    /// Статус сессии для UI.
    pub fn status_for(&self, exe_path: Option<&str>, is_system: bool) -> AppStatus {
        if is_system {
            return AppStatus::System;
        }
        match exe_path {
            Some(path) => match self.known.get(path) {
                Some(k) if k.trusted => AppStatus::Trusted,
                Some(k) if k.default_applied => AppStatus::Defaulted,
                _ => AppStatus::Defaulted, // появилось только что — дефолт уже отправлен
            },
            None => AppStatus::Unidentified,
        }
    }

    /// Обработать вход и вернуть список действий.
    pub fn handle(&mut self, input: PolicyInput, now: Instant) -> Vec<PolicyAction> {
        let mut actions = Vec::new();
        match input {
            PolicyInput::SessionAppeared(session) => self.on_session_appeared(session, now, &mut actions),
            PolicyInput::SessionVolumeChanged { key, by_user, .. } => {
                // Пользователь (или само приложение) взял громкость под контроль —
                // отменяем запланированные повторы, чтобы не воевать с ним.
                if by_user {
                    self.reasserts.retain(|r| r.key != key);
                }
            }
            PolicyInput::SessionGone(key) => {
                self.session_paths.remove(&key);
                self.reasserts.retain(|r| r.key != key);
            }
            PolicyInput::Tick(tick_at) => {
                let due: Vec<ReAssert> = self
                    .reasserts
                    .iter()
                    .filter(|r| r.due <= tick_at)
                    .cloned()
                    .collect();
                for r in due {
                    actions.push(PolicyAction::SetVolume {
                        key: r.key,
                        volume: r.volume,
                        reason: "re-assert",
                    });
                }
                self.reasserts.retain(|r| r.due > tick_at);
            }
            PolicyInput::ConfigUpdated(config) => {
                // Обновляем цели отложенных re-assert, если они ссылались на старый дефолт.
                let old_default = self.config.unknown_app_volume;
                let new_default = config.clamped_unknown_volume();
                for r in &mut self.reasserts {
                    if (r.volume - old_default).abs() < f32::EPSILON {
                        r.volume = new_default;
                    }
                }
                self.config = *config;
            }
            PolicyInput::TrustApp { exe_path, trusted } => {
                let entry = self.known.entry(exe_path.clone()).or_insert_with(|| KnownApp {
                    trusted: false,
                    default_applied: false,
                    display_name: display_name_of(&exe_path),
                });
                entry.trusted = trusted;
                if trusted {
                    // Доверенному не применяем и отложенные повторы.
                    self.reasserts.retain(|r| {
                        self.session_paths
                            .get(&r.key)
                            .map(|p| p != &exe_path)
                            .unwrap_or(true)
                    });
                }
                actions.push(PolicyAction::SaveKnown);
            }
            PolicyInput::ForgetApp { exe_path } => {
                self.known.remove(&exe_path);
                // Сессии этого приложения, играющие прямо сейчас, снова считаем
                // неизвестными: применяем дефолт немедленно, старые повторы отменяем.
                let keys: Vec<SessionKey> = self
                    .session_paths
                    .iter()
                    .filter(|(_, p)| **p == exe_path)
                    .map(|(k, _)| k.clone())
                    .collect();
                self.reasserts.retain(|r| !keys.contains(&r.key));
                for key in keys {
                    self.apply_default_to(&key, now, &mut actions);
                }
                actions.push(PolicyAction::SaveKnown);
            }
        }
        actions
    }

    fn on_session_appeared(&mut self, session: SessionInfo, now: Instant, actions: &mut Vec<PolicyAction>) {
        // Системные звуки Windows — не «приложение», движок их не трогает никогда.
        if session.is_system {
            return;
        }
        let Some(path) = session.exe_path.clone() else {
            return; // не удалось опознать процесс — безопаснее ничего не менять
        };

        let unknown = match self.known.get(&path) {
            Some(k) if k.trusted => false,
            Some(k) if k.default_applied => false,
            _ => true,
        };
        if !unknown {
            self.session_paths.insert(session.key, path);
            return;
        }

        let entry = self.known.entry(path.clone()).or_insert_with(|| KnownApp {
            trusted: false,
            default_applied: false,
            display_name: session.display_name.clone(),
        });
        entry.default_applied = true;
        entry.display_name = session.display_name.clone();
        self.session_paths.insert(session.key.clone(), path);
        actions.push(PolicyAction::SaveKnown);

        self.apply_default_to(&session.key, now, actions);
    }

    /// Поставить дефолтную громкость сессии прямо сейчас + расписание повторов.
    fn apply_default_to(&mut self, key: &SessionKey, now: Instant, actions: &mut Vec<PolicyAction>) {
        let volume = self.config.clamped_unknown_volume();
        actions.push(PolicyAction::SetVolume {
            key: key.clone(),
            volume,
            reason: "unknown app",
        });
        self.schedule_reasserts(key, volume, now);
    }

    fn schedule_reasserts(&mut self, key: &SessionKey, volume: f32, now: Instant) {
        for delay_ms in &self.config.re_assert_delays_ms {
            self.reasserts.push(ReAssert {
                due: now + Duration::from_millis(*delay_ms),
                key: key.clone(),
                volume,
            });
        }
    }

    /// Снять все отложенные действия (например, при перепривязке устройства).
    pub fn reset_pending(&mut self) {
        self.reasserts.clear();
        self.session_paths.clear();
    }
}

/// Имя exe из полного пути (для вставки в базу, когда сессии уже нет).
pub fn display_name_of(exe_path: &str) -> String {
    std::path::Path::new(exe_path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| exe_path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config {
            re_assert_delays_ms: vec![100, 200],
            ..Config::default()
        }
    }

    fn session(key: &str, path: Option<&str>) -> SessionInfo {
        SessionInfo {
            key: key.to_string(),
            pid: 42,
            exe_path: path.map(|p| p.to_string()),
            display_name: "game.exe".to_string(),
            is_system: false,
            volume: 1.0,
            mute: false,
            active: true,
        }
    }

    #[test]
    fn unknown_app_gets_default_volume_and_known_entry() {
        let mut e = RuleEngine::new(cfg(), KnownMap::new());
        let t0 = Instant::now();
        let actions = e.handle(PolicyInput::SessionAppeared(session("s1", Some(r"c:\g\game.exe"))), t0);

        assert_eq!(
            actions,
            vec![
                PolicyAction::SaveKnown,
                PolicyAction::SetVolume { key: "s1".into(), volume: 0.05, reason: "unknown app" },
            ]
        );
        let known = e.known().get(r"c:\g\game.exe").unwrap();
        assert!(known.default_applied);
        assert_eq!(e.reasserts.len(), 2); // расписание повторов создано
    }

    #[test]
    fn known_app_is_not_touched() {
        let mut e = RuleEngine::new(cfg(), KnownMap::new());
        let t0 = Instant::now();
        e.handle(PolicyInput::SessionAppeared(session("s1", Some(r"c:\g\game.exe"))), t0);
        // Вторая сессия того же приложения: дефолт уже применялся.
        let actions = e.handle(PolicyInput::SessionAppeared(session("s2", Some(r"c:\g\game.exe"))), t0);
        assert!(actions.is_empty());
        assert_eq!(e.reasserts.len(), 2); // повторов для s2 нет
    }

    #[test]
    fn trusted_app_is_not_touched() {
        let mut known = KnownMap::new();
        known.insert(
            r"c:\g\music.exe".to_string(),
            KnownApp { trusted: true, default_applied: true, display_name: "music.exe".into() },
        );
        let mut e = RuleEngine::new(cfg(), known);
        let actions = e.handle(
            PolicyInput::SessionAppeared(session("s1", Some(r"c:\g\music.exe"))),
            Instant::now(),
        );
        assert!(actions.is_empty());
    }

    #[test]
    fn re_assert_fires_on_time_and_user_change_cancels_it() {
        let mut e = RuleEngine::new(cfg(), KnownMap::new());
        let t0 = Instant::now();
        e.handle(PolicyInput::SessionAppeared(session("s1", Some(r"c:\g\game.exe"))), t0);

        // Пользователь поднял громкость через микшер до того, как сработали повторы.
        e.handle(
            PolicyInput::SessionVolumeChanged { key: "s1".into(), volume: 0.8, mute: false, by_user: true },
            t0,
        );
        let actions = e.handle(PolicyInput::Tick(t0 + Duration::from_millis(300)), Instant::now());
        assert!(actions.is_empty(), "пользовательские изменения отменяют re-assert");
    }

    #[test]
    fn re_assert_fires_when_nobody_touched_volume() {
        let mut e = RuleEngine::new(cfg(), KnownMap::new());
        let t0 = Instant::now();
        e.handle(PolicyInput::SessionAppeared(session("s1", Some(r"c:\g\game.exe"))), t0);

        let actions = e.handle(PolicyInput::Tick(t0 + Duration::from_millis(150)), Instant::now());
        assert_eq!(
            actions,
            vec![PolicyAction::SetVolume { key: "s1".into(), volume: 0.05, reason: "re-assert" }]
        );
        // Второй повтор — на 200 мс.
        let actions = e.handle(PolicyInput::Tick(t0 + Duration::from_millis(250)), Instant::now());
        assert_eq!(actions.len(), 1);
        // Больше повторов нет.
        let actions = e.handle(PolicyInput::Tick(t0 + Duration::from_millis(1000)), Instant::now());
        assert!(actions.is_empty());
    }

    #[test]
    fn system_sounds_are_skipped() {
        let mut e = RuleEngine::new(cfg(), KnownMap::new());
        let mut s = session("sys", None);
        s.is_system = true;
        let actions = e.handle(PolicyInput::SessionAppeared(s), Instant::now());
        assert!(actions.is_empty());
        assert!(e.known().is_empty());
    }

    #[test]
    fn unidentified_process_is_skipped() {
        let mut e = RuleEngine::new(cfg(), KnownMap::new());
        let actions = e.handle(PolicyInput::SessionAppeared(session("s1", None)), Instant::now());
        assert!(actions.is_empty());
    }

    #[test]
    fn forget_applies_default_to_running_session() {
        let mut e = RuleEngine::new(cfg(), KnownMap::new());
        let t0 = Instant::now();
        e.handle(PolicyInput::SessionAppeared(session("s1", Some(r"c:\g\game.exe"))), t0);
        e.known.remove(r"c:\g\game.exe"); // симулируем сброс базы

        let actions = e.handle(PolicyInput::ForgetApp { exe_path: r"c:\g\game.exe".into() }, t0);
        assert_eq!(
            actions,
            vec![
                PolicyAction::SetVolume { key: "s1".into(), volume: 0.05, reason: "unknown app" },
                PolicyAction::SaveKnown,
            ]
        );
        assert_eq!(e.reasserts.len(), 2);
    }

    #[test]
    fn trust_cancels_pending_re_asserts_of_that_app() {
        let mut e = RuleEngine::new(cfg(), KnownMap::new());
        let t0 = Instant::now();
        e.handle(PolicyInput::SessionAppeared(session("s1", Some(r"c:\g\game.exe"))), t0);
        assert_eq!(e.reasserts.len(), 2);

        let actions = e.handle(
            PolicyInput::TrustApp { exe_path: r"c:\g\game.exe".into(), trusted: true },
            t0,
        );
        assert_eq!(actions, vec![PolicyAction::SaveKnown]);
        assert!(e.reasserts.is_empty());
        assert!(e.known().get(r"c:\g\game.exe").unwrap().trusted);
    }

    #[test]
    fn config_update_retargets_pending_re_asserts() {
        let mut e = RuleEngine::new(cfg(), KnownMap::new());
        let t0 = Instant::now();
        e.handle(PolicyInput::SessionAppeared(session("s1", Some(r"c:\g\game.exe"))), t0);

        let new_cfg = Config { unknown_app_volume: 0.1, ..cfg() };
        e.handle(PolicyInput::ConfigUpdated(Box::new(new_cfg)), t0);
        let actions = e.handle(PolicyInput::Tick(t0 + Duration::from_millis(150)), Instant::now());
        assert_eq!(
            actions,
            vec![PolicyAction::SetVolume { key: "s1".into(), volume: 0.1, reason: "re-assert" }]
        );
    }

    #[test]
    fn session_gone_cleans_up() {
        let mut e = RuleEngine::new(cfg(), KnownMap::new());
        let t0 = Instant::now();
        e.handle(PolicyInput::SessionAppeared(session("s1", Some(r"c:\g\game.exe"))), t0);
        e.handle(PolicyInput::SessionGone("s1".into()), t0);
        assert!(e.reasserts.is_empty());
        assert!(e.session_paths.is_empty());
        // База известных приложений сохраняется — дефолт уже не применится.
        assert!(e.known().contains_key(r"c:\g\game.exe"));
    }
}
