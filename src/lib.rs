//! Make It Sound Same — утилита контроля громкости приложений Windows.
//!
//! Модули:
//! - [`audio`] — WASAPI-бэкенд: перечисление/события аудиосессий, управление громкостью;
//! - [`policy`] — движок правил (дефолт для неизвестных приложений) + каркас v2 (громкость);
//! - [`config`] — TOML-конфиг и база известных приложений в `%APPDATA%`;
//! - [`ui`] — окно на egui + иконка в трее;
//! - [`autostart`] — автозапуск через HKCU\\...\\Run.

pub mod audio;
pub mod autostart;
pub mod config;
pub mod console;
pub mod logging;
pub mod policy;
pub mod ui;

/// Точка входа приложения (вызывается из `main.rs`).
/// `debug` — флаг `--debug`: логи дублируются в консоль.
pub fn run(debug: bool) -> anyhow::Result<()> {
    ui::run_app(debug)?;
    Ok(())
}
