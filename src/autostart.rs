//! Автозапуск вместе с Windows через ключ реестра
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.

use anyhow::{Context, Result};
use winreg::enums::{HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE};
use winreg::RegKey;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "MakeItSoundSame";

/// Включён ли автозапуск.
pub fn is_enabled() -> bool {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    hkcu.open_subkey_with_flags(RUN_KEY, KEY_QUERY_VALUE)
        .and_then(|k| k.get_value::<String, _>(VALUE_NAME))
        .is_ok()
}

/// Включить/выключить автозапуск (пишет путь текущего exe в кавычках).
pub fn set_enabled(enable: bool) -> Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu
        .open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE)
        .context("открытие ключа автозапуска")?;
    if enable {
        let exe = std::env::current_exe().context("путь текущего exe")?;
        key.set_value(VALUE_NAME, &format!("\"{}\"", exe.display()))
            .context("запись значения автозапуска")?;
        tracing::info!("автозапуск включён: {}", exe.display());
    } else {
        key.delete_value(VALUE_NAME)
            .context("удаление значения автозапуска")?;
        tracing::info!("автозапуск выключен");
    }
    Ok(())
}
