//! Конфигурация и база известных приложений (TOML в `%APPDATA%\make-it-sound-same`).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// Настройки приложения (`config.toml`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Громкость, ставимая «неизвестному» приложению (0.0..=1.0). По умолчанию 5%.
    pub unknown_app_volume: f32,
    /// Задержки повторного применения дефолта после появления сессии, мс
    /// (некоторые приложения сами перезаписывают громкость в первые секунды).
    pub re_assert_delays_ms: Vec<u64>,
    /// Скрывать сессию системных звуков Windows из списка приложений.
    pub exclude_system_sounds: bool,
    /// Запускать при входе в Windows (HKCU\...\Run).
    pub autostart: bool,
    /// Открывать окно свёрнутым в трей при старте.
    pub start_minimized: bool,
    /// Закрытие окна — сворачивание в трей вместо выхода.
    pub close_to_tray: bool,
    /// Масштаб интерфейса (множитель поверх масштаба ОС): 1.0 = 100%.
    pub ui_scale: f32,
    /// Автоприглушение громких приложений (v2).
    pub loudness_enabled: bool,
    /// Насколько дБ выше комфортного уровня разрешено играть.
    pub loudness_ceiling_offset_db: f32,
    /// Опорный («комфортный») уровень в dBFS — заполняется калибровкой.
    pub loudness_reference_dbfs: Option<f32>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            unknown_app_volume: 0.05,
            re_assert_delays_ms: vec![300, 1000, 2000],
            exclude_system_sounds: true,
            autostart: false,
            start_minimized: false,
            close_to_tray: true,
            ui_scale: 1.25,
            loudness_enabled: true,
            loudness_ceiling_offset_db: 5.0,
            loudness_reference_dbfs: None,
        }
    }
}

impl Config {
    /// Громкость неизвестного приложения, зажатая в допустимые пределы.
    pub fn clamped_unknown_volume(&self) -> f32 {
        self.unknown_app_volume.clamp(0.0, 1.0)
    }
}

/// Запись о приложении (`known-apps.toml`), ключ — полный путь к exe в нижнем регистре.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnownApp {
    /// Доверенное: движок никогда не трогает громкость, даже если добавить заново.
    pub trusted: bool,
    /// Дефолт уже применялся (при следующем запуске громкость не сбрасывается).
    pub default_applied: bool,
    /// Кешированное имя exe (для списка после удаления приложения).
    pub display_name: String,
}

/// База известных приложений: путь exe → запись.
pub type KnownMap = BTreeMap<String, KnownApp>;

#[derive(Debug, Default, Serialize, Deserialize)]
struct KnownFile {
    apps: KnownMap,
}

/// Пути к файлам приложения.
#[derive(Debug, Clone)]
pub struct AppPaths {
    pub dir: PathBuf,
    pub config: PathBuf,
    pub known: PathBuf,
    pub log: PathBuf,
}

/// Каталог и файлы конфигурации: `%APPDATA%\make-it-sound-same`.
pub fn paths() -> anyhow::Result<AppPaths> {
    let dir = dirs::data_dir()
        .context("не удалось определить %APPDATA%")?
        .join("make-it-sound-same");
    fs::create_dir_all(&dir).with_context(|| format!("создание каталога {}", dir.display()))?;
    Ok(AppPaths {
        config: dir.join("config.toml"),
        known: dir.join("known-apps.toml"),
        log: dir.join("make-it-sound-same.log"),
        dir,
    })
}

/// Загрузка конфига; при отсутствии/повреждении файла — значения по умолчанию.
pub fn load_config(path: &Path) -> Config {
    match fs::read_to_string(path) {
        Ok(text) => match toml::from_str(&text) {
            Ok(cfg) => cfg,
            Err(e) => {
                tracing::warn!("config.toml повреждён ({e}); использую значения по умолчанию");
                Config::default()
            }
        },
        Err(_) => Config::default(),
    }
}

/// Загрузка базы известных приложений; при отсутствии файла — пустая база.
pub fn load_known(path: &Path) -> KnownMap {
    match fs::read_to_string(path) {
        Ok(text) => match toml::from_str::<KnownFile>(&text) {
            Ok(f) => f.apps,
            Err(e) => {
                tracing::warn!("known-apps.toml повреждён ({e}); начинаю с пустой базы");
                KnownMap::new()
            }
        },
        Err(_) => KnownMap::new(),
    }
}

/// Сохранение конфига (атомарно: tmp → copy → remove tmp).
pub fn save_config(path: &Path, config: &Config) -> anyhow::Result<()> {
    save_toml(path, &config)
}

/// Сохранение базы известных приложений (атомарно).
pub fn save_known(path: &Path, known: &KnownMap) -> anyhow::Result<()> {
    save_toml(path, &KnownFile { apps: known.clone() })
}

fn save_toml<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    let text = toml::to_string_pretty(value).context("сериализация TOML")?;
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, text).with_context(|| format!("запись {}", tmp.display()))?;
    // std::fs::rename на Windows не перезаписывает существующий файл, поэтому copy.
    fs::copy(&tmp, path).with_context(|| format!("замена {}", path.display()))?;
    let _ = fs::remove_file(&tmp);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrip() {
        let cfg = Config {
            unknown_app_volume: 0.07,
            re_assert_delays_ms: vec![100, 500],
            exclude_system_sounds: false,
            autostart: true,
            start_minimized: true,
            close_to_tray: false,
            ui_scale: 1.5,
            loudness_enabled: true,
            loudness_ceiling_offset_db: 4.0,
            loudness_reference_dbfs: Some(-18.0),
        };
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn config_defaults_fill_missing_fields() {
        let back: Config = toml::from_str("").unwrap();
        assert_eq!(back, Config::default());
    }

    #[test]
    fn known_roundtrip_with_windows_paths() {
        let mut apps = KnownMap::new();
        apps.insert(
            r"c:\games\foo\foo.exe".to_string(),
            KnownApp {
                trusted: false,
                default_applied: true,
                display_name: "foo.exe".to_string(),
            },
        );
        let text = toml::to_string_pretty(&KnownFile { apps: apps.clone() }).unwrap();
        let back: KnownFile = toml::from_str(&text).unwrap();
        assert_eq!(back.apps, apps);
    }

    #[test]
    fn save_load_atomic() {
        let dir = std::env::temp_dir().join("miss-test-config");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cfg.toml");
        let cfg = Config {
            unknown_app_volume: 0.2,
            ..Config::default()
        };
        save_config(&path, &cfg).unwrap();
        // Повторная запись (проверяем перезапись поверх существующего файла).
        save_config(&path, &Config::default()).unwrap();
        assert_eq!(load_config(&path), Config::default());
        let _ = fs::remove_file(&path);
    }
}
