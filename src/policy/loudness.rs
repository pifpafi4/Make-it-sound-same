//! Каркас v2: авто-приглушение громких сессий относительно «комфортного» уровня.
//!
//! Физические dB SPL без микрофона измерить нельзя, поэтому v2 работает с
//! **относительной шкалой**: пользователь калибруется на комфортной громкости,
//! утилита запоминает опорный уровень в dBFS (относительно цифрового максимума),
//! и приглушает сессии, чей измеренный уровень превышает опору + потолок дБ.
//!
//! В v1 модуль **не подключён** к живому аудио — только математика и типы,
//! покрытые тестами. Подключение (v2):
//! 1. polling `IAudioMeterInformation::GetPeakValue` по каждой сессии (~20 Гц);
//! 2. `Smoother` сглаживает пик в уровень dBFS;
//! 3. `required_gain_db` → уменьшение громкости сессии с плавным attack/release.

/// Порог отсечения логарифма: -100 dBFS.
pub const MIN_PEAK: f32 = 1e-5;

/// Пик (0.0..=1.0, амплитуда) → dBFS. 1.0 = 0 dBFS, 0.5 ≈ -6 dBFS.
pub fn peak_to_dbfs(peak: f32) -> f32 {
    20.0 * peak.max(MIN_PEAK).log10()
}

/// Прибавка фейдера громкости в дБ. Диапазон зажат снизу 0.001 (−60 дБ),
/// чтобы нулевая громкость не превращала уровень в −inf.
pub fn fader_gain_db(volume: f32) -> f32 {
    20.0 * volume.clamp(0.001, 1.0).log10()
}

/// Обратное преобразование: dBFS → амплитуда 0..=1.
pub fn dbfs_to_linear(dbfs: f32) -> f32 {
    10.0f32.powf(dbfs / 20.0)
}

/// Экспоненциальное сглаживание уровня (быстрая реакция вверх, плавное вниз).
#[derive(Debug, Clone)]
pub struct Smoother {
    /// Коэффициент сглаживания вверх (attack), 0..=1. Больше — быстрее.
    pub alpha_attack: f32,
    /// Коэффициент сглаживания вниз (release), 0..=1.
    pub alpha_release: f32,
    ema: Option<f32>,
}

impl Smoother {
    pub fn new(alpha_attack: f32, alpha_release: f32) -> Self {
        Self {
            alpha_attack: alpha_attack.clamp(0.0, 1.0),
            alpha_release: alpha_release.clamp(0.0, 1.0),
            ema: None,
        }
    }

    /// Затолкать новый пик (амплитуда), получить сглаженный уровень в dBFS.
    pub fn update(&mut self, peak: f32) -> f32 {
        let dbfs = peak_to_dbfs(peak);
        self.ema = Some(match self.ema {
            None => dbfs,
            // Вверх (громче) реагируем быстрее, чем отпускаем вниз.
            Some(prev) if dbfs > prev => prev + (dbfs - prev) * self.alpha_attack,
            Some(prev) => prev + (dbfs - prev) * self.alpha_release,
        });
        self.ema.unwrap()
    }

    pub fn current(&self) -> Option<f32> {
        self.ema
    }

    pub fn reset(&mut self) {
        self.ema = None;
    }
}

/// Настройки лимитера (v2).
#[derive(Debug, Clone)]
pub struct LoudnessConfig {
    /// Насколько дБ выше опоры разрешено играть (например, 5.0 → «слушаю на 70, глушу 75+»).
    pub ceiling_offset_db: f32,
    /// Сколько дБ можно снизить максимум за раз (защита от резких скачков).
    pub max_duck_db: f32,
}

impl Default for LoudnessConfig {
    fn default() -> Self {
        Self {
            ceiling_offset_db: 5.0,
            max_duck_db: 12.0,
        }
    }
}

/// Насколько дБ нужно приглушить сессию, чтобы попасть под потолок.
/// `None` — сессия и так тише потолка.
pub fn required_gain_db(measured_dbfs: f32, reference_dbfs: f32, cfg: &LoudnessConfig) -> Option<f32> {
    let ceiling = reference_dbfs + cfg.ceiling_offset_db;
    let excess = measured_dbfs - ceiling;
    if excess <= 0.0 {
        None
    } else {
        Some(excess.min(cfg.max_duck_db))
    }
}

// ----------------------------------------------------------------------------
// Лимитер v2: держит состояние приглушения по сессиям, возвращает действия.
// ----------------------------------------------------------------------------

/// Гистерезис отпускания: отпускаем, только когда уровень упал ниже
/// «потолок − 2 дБ», чтобы не дёргаться на границе.
pub const RELEASE_HYSTERESIS_DB: f32 = 2.0;
/// Доля пути к исходной громкости за один тик при отпускании (плавный возврат).
pub const RELEASE_STEP: f32 = 0.2;
/// Ниже этого уровня (dBFS) сессия считается тихой и лимитером игнорируется.
pub const SILENCE_FLOOR_DBFS: f32 = -80.0;

/// Действие лимитера для сессии.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DuckAction {
    /// Ничего не менять (не дёргаем COM без необходимости).
    None,
    /// Установить громкость сессии. `ducking` — приглушена ли прямо сейчас.
    Set { volume: f32, ducking: bool },
}

/// Состояние одной сессии в лимитере.
#[derive(Debug, Clone)]
struct LimiterState {
    /// Громкость, выбранная пользователем (или правилом дефолта) — к ней возвращаемся.
    base_volume: f32,
    /// Текущий коэффициент приглушения (линейный, 1.0 = не приглушено).
    duck_linear: f32,
    /// Последняя реально применённая громкость (чтобы не дёргать COM без изменений).
    last_volume: f32,
}

/// Порог значимого изменения громкости (доля 0..1): меньше — считаем неизменной.
const VOLUME_EPSILON: f32 = 0.002;

/// Ограничитель громкости: следит за уровнями сессий и приглушает те,
/// что играют громче «потолка» (опора + смещение), плавно возвращая громкость.
#[derive(Debug, Default)]
pub struct Limiter {
    states: std::collections::BTreeMap<String, LimiterState>,
}

impl Limiter {
    /// Запомнить «базовую» громкость сессии (пользователь/правило её изменили).
    pub fn set_base(&mut self, key: &str, volume: f32) {
        if let Some(st) = self.states.get_mut(key) {
            st.base_volume = volume.clamp(0.0, 1.0);
        }
    }

    pub fn base_of(&self, key: &str) -> Option<f32> {
        self.states.get(key).map(|st| st.base_volume)
    }

    /// Приглушена ли сессия прямо сейчас.
    pub fn is_ducked(&self, key: &str) -> bool {
        self.states
            .get(key)
            .map(|st| st.duck_linear < 0.999)
            .unwrap_or(false)
    }

    pub fn remove(&mut self, key: &str) {
        self.states.remove(key);
    }

    pub fn clear(&mut self) {
        self.states.clear();
    }

    /// Все приглушённые сессии с их базовой громкостью (для отката при выключении).
    pub fn ducked(&self) -> Vec<(String, f32)> {
        self.states
            .iter()
            .filter(|(_, st)| st.duck_linear < 0.999)
            .map(|(k, st)| (k.clone(), st.base_volume))
            .collect()
    }

    /// Обработать новый сглаженный уровень сессии. `current_volume` — громкость
    /// сессии прямо сейчас (нужна только при первом появлении сессии).
    pub fn update(
        &mut self,
        key: &str,
        level_dbfs: f32,
        current_volume: f32,
        cfg: &LoudnessConfig,
        reference_dbfs: f32,
    ) -> DuckAction {
        let state = self.states.entry(key.to_string()).or_insert(LimiterState {
            base_volume: current_volume.clamp(0.0, 1.0),
            duck_linear: 1.0,
            last_volume: f32::NAN,
        });

        let ceiling = reference_dbfs + cfg.ceiling_offset_db;
        let release_below = ceiling - RELEASE_HYSTERESIS_DB;
        let base = state.base_volume;

        // Отправляем Set только если громкость реально изменилась.
        let changed = |state: &LimiterState, volume: f32| {
            volume.is_nan()
                || state.last_volume.is_nan()
                || (volume - state.last_volume).abs() > VOLUME_EPSILON
        };

        if state.duck_linear >= 0.999 {
            // Не приглушена: включаемся только при явном превышении потолка.
            if level_dbfs <= ceiling || level_dbfs <= SILENCE_FLOOR_DBFS {
                return DuckAction::None;
            }
            let gain = required_gain_db(level_dbfs, reference_dbfs, cfg).unwrap_or(0.0);
            state.duck_linear = dbfs_to_linear(-gain);
            let volume = base * state.duck_linear;
            if !changed(state, volume) {
                return DuckAction::None;
            }
            state.last_volume = volume;
            DuckAction::Set { volume, ducking: true }
        } else {
            // Приглушена: отпускаем ниже «потолок − гистерезис», иначе держим/усиливаем.
            if level_dbfs < release_below {
                state.duck_linear += (1.0 - state.duck_linear) * RELEASE_STEP;
                if state.duck_linear >= 0.999 {
                    state.duck_linear = 1.0;
                    state.last_volume = base;
                    DuckAction::Set { volume: base, ducking: false }
                } else {
                    let volume = base * state.duck_linear;
                    if !changed(state, volume) {
                        return DuckAction::None;
                    }
                    state.last_volume = volume;
                    DuckAction::Set { volume, ducking: true }
                }
            } else {
                // Всё ещё громко: углубляем прижим, если нужно (быстрый attack).
                if let Some(gain) = required_gain_db(level_dbfs, reference_dbfs, cfg) {
                    let target = dbfs_to_linear(-gain);
                    if target < state.duck_linear {
                        state.duck_linear = target;
                    }
                }
                let volume = base * state.duck_linear;
                if !changed(state, volume) {
                    return DuckAction::None;
                }
                state.last_volume = volume;
                DuckAction::Set { volume, ducking: true }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dbfs_conversions() {
        assert!((peak_to_dbfs(1.0)).abs() < 1e-4);
        assert!((peak_to_dbfs(0.5) - (-6.0206)).abs() < 1e-3);
        assert!((peak_to_dbfs(0.0) - (-100.0)).abs() < 1e-3);
        assert!((dbfs_to_linear(-6.0206) - 0.5).abs() < 1e-3);
        // Круговое преобразование.
        let db = -23.7;
        assert!((peak_to_dbfs(dbfs_to_linear(db)) - db).abs() < 1e-3);
    }

    #[test]
    fn fader_gain() {
        assert!((fader_gain_db(1.0)).abs() < 1e-4);
        assert!((fader_gain_db(0.1) - (-20.0)).abs() < 1e-3);
        assert!((fader_gain_db(0.5) - (-6.0206)).abs() < 1e-3);
        // Ноль не уходит в -inf, зажимается на -60 дБ.
        assert!((fader_gain_db(0.0) - (-60.0)).abs() < 1e-3);
        // Слышимый уровень = метр потока + фейдер сессии + фейдер устройства.
        let audible = peak_to_dbfs(1.0) + fader_gain_db(0.1) + fader_gain_db(1.0);
        assert!((audible - (-20.0)).abs() < 1e-3);
    }

    #[test]
    fn smoother_rises_fast_and_falls_slow() {
        // Мгновенный подъём при alpha_attack = 1.
        let mut s = Smoother::new(1.0, 0.1);
        let loud = s.update(1.0);
        assert!((loud - 0.0).abs() < 1e-4);

        // Частичный подъём при малом alpha (первый сэмпл только инициализирует).
        let mut s = Smoother::new(0.3, 0.3);
        let level0 = s.update(0.0);
        assert!((level0 - (-100.0)).abs() < 1e-3, "инициализация: {level0}");
        let mut level = s.update(1.0);
        assert!((level - (-70.0)).abs() < 1e-3, "частичный шаг: {level}");
        for _ in 0..50 {
            level = s.update(1.0);
        }
        assert!((level - 0.0).abs() < 0.5, "должен сойтись к 0 dBFS: {level}");

        // Отпускание медленнее: за один шаг вниз с alpha_release=0.1 уровень
        // уходит лишь на 10% пути от 0 к -100 dBFS.
        let mut s = Smoother::new(1.0, 0.1);
        s.update(1.0);
        let after_drop = s.update(0.0);
        assert!((after_drop - (-10.0)).abs() < 0.5, "плавное отпускание: {after_drop}");
    }

    #[test]
    fn required_gain_only_above_ceiling() {
        let cfg = LoudnessConfig::default();
        // Опора: пользователь калибровался на -20 dBFS, потолок = -15.
        assert_eq!(required_gain_db(-30.0, -20.0, &cfg), None);
        assert_eq!(required_gain_db(-15.5, -20.0, &cfg), None); // ниже потолка
        assert_eq!(required_gain_db(-14.5, -20.0, &cfg), Some(0.5)); // на 0.5 дБ выше
        assert_eq!(required_gain_db(-10.0, -20.0, &cfg), Some(5.0)); // на 5 дБ выше
        // Ограничение максимального приглушения.
        assert_eq!(required_gain_db(0.0, -20.0, &cfg), Some(12.0));
    }

    // --- Лимитер ---

    fn cfg() -> LoudnessConfig {
        LoudnessConfig { ceiling_offset_db: 5.0, max_duck_db: 12.0 }
    }

    #[test]
    fn limiter_engages_above_ceiling_and_caps_at_max_duck() {
        let mut l = Limiter::default();
        // Опора -20, потолок -15. Уровень -14 → прижать на 1 дБ.
        let a = l.update("s1", -14.0, 1.0, &cfg(), -20.0);
        match a {
            DuckAction::Set { volume, ducking } => {
                assert!(ducking);
                assert!((volume - dbfs_to_linear(-1.0)).abs() < 1e-4);
            }
            DuckAction::None => panic!("должен приглушить"),
        }
        // Очень громкий уровень: прижим ограничен max_duck (12 дБ).
        let a = l.update("s2", 0.0, 1.0, &cfg(), -20.0);
        match a {
            DuckAction::Set { volume, .. } => {
                assert!((volume - dbfs_to_linear(-12.0)).abs() < 1e-4);
            }
            DuckAction::None => panic!("должен приглушить"),
        }
    }

    #[test]
    fn limiter_silent_below_ceiling() {
        let mut l = Limiter::default();
        assert_eq!(l.update("s1", -30.0, 1.0, &cfg(), -20.0), DuckAction::None);
        assert_eq!(l.update("s1", -16.0, 1.0, &cfg(), -20.0), DuckAction::None);
        // Тишина вообще не триггерит.
        assert_eq!(l.update("s2", -100.0, 1.0, &cfg(), -20.0), DuckAction::None);
    }

    #[test]
    fn limiter_releases_with_hysteresis_and_returns_to_base() {
        let mut l = Limiter::default();
        l.update("s1", -10.0, 0.8, &cfg(), -20.0); // прижали
        assert!(l.is_ducked("s1"));

        // На границе потолка ещё держим (гистерезис: отпускание ниже -17).
        // «Держим» может быть как Set(ducking=true), так и None — без изменений.
        match l.update("s1", -16.0, 0.8, &cfg(), -20.0) {
            DuckAction::Set { ducking, .. } => assert!(ducking),
            DuckAction::None => assert!(l.is_ducked("s1"), "гистерезис должен держать прижим"),
        }

        // Ниже границы: постепенный возврат к базовым 0.8.
        // (Шаги меньше эпсилона честно возвращают None — пропускаем их.)
        let mut last = 0.0f32;
        for _ in 0..100 {
            match l.update("s1", -40.0, 0.8, &cfg(), -20.0) {
                DuckAction::Set { volume, ducking } => {
                    assert!(volume > last - f32::EPSILON, "возврат монотонный");
                    last = volume;
                    if !ducking {
                        assert!((volume - 0.8).abs() < 1e-4, "вернулись к базе");
                        return;
                    }
                }
                DuckAction::None => {}
            }
        }
        panic!("за 100 тиков не вернулись к базе");
    }

    #[test]
    fn limiter_base_follows_user_change_while_ducked() {
        let mut l = Limiter::default();
        l.update("s1", -10.0, 1.0, &cfg(), -20.0); // прижали от базы 1.0
        l.set_base("s1", 0.5); // пользователь поставил 50% во время прижима
        match l.update("s1", -10.0, 0.5, &cfg(), -20.0) {
            DuckAction::Set { volume, ducking } => {
                assert!(ducking);
                // Прижим остался тем же, но относительно новой базы.
                let expected = 0.5 * dbfs_to_linear(-required_gain_db(-10.0, -20.0, &cfg()).unwrap());
                assert!((volume - expected).abs() < 1e-4);
            }
            DuckAction::None => panic!("должен держать прижим"),
        }
    }

    #[test]
    fn limiter_ducked_list_and_disable() {
        let mut l = Limiter::default();
        l.update("s1", -10.0, 0.9, &cfg(), -20.0);
        l.update("s2", -30.0, 0.7, &cfg(), -20.0); // не приглушён
        let ducked = l.ducked();
        assert_eq!(ducked.len(), 1);
        assert_eq!(ducked[0].0, "s1");
        assert!((ducked[0].1 - 0.9).abs() < 1e-4);
    }

    #[test]
    fn limiter_reengages_after_full_release() {
        let mut l = Limiter::default();
        l.update("s1", -10.0, 0.8, &cfg(), -20.0);
        // Отпустили полностью.
        for _ in 0..100 {
            if let DuckAction::Set { ducking: false, .. } =
                l.update("s1", -40.0, 0.8, &cfg(), -20.0)
            {
                break;
            }
        }
        assert!(!l.is_ducked("s1"));
        // И снова прижались при новом превышении.
        match l.update("s1", -10.0, 0.8, &cfg(), -20.0) {
            DuckAction::Set { ducking: true, .. } => {}
            other => panic!("должен снова прижать: {other:?}"),
        }
    }
}
