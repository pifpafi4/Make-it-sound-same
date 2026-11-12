//! Консоль для режима отладки (`--debug`).
//!
//! Бинарь собран с `windows_subsystem = "gui"`: при обычном запуске консоль
//! не открывается вообще. С флагом `--debug` логи дублируются в консоль:
//! - если stdout/stderr уже валидны (перенаправление в файл/пайп, ConPTY) — пишем как есть;
//! - если запуск из терминала (cmd/PowerShell/Windows Terminal) — цепляемся к его консоли;
//! - если консоли нет вовсе (двойной клик по exe) — создаём собственную.

use windows::core::w;
use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Console::{
    AllocConsole, AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS,
    STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};

/// Открыть консоль для логов, если передан флаг `--debug`.
/// Возвращает true в режиме отладки (нужен для включения консольного слоя логов).
pub fn attach_if_debug() -> bool {
    if !std::env::args().any(|a| a == "--debug") {
        return false;
    }

    // Дефолтный уровень логов для отладки (если пользователь не задал свой).
    if std::env::var_os("RUST_LOG").is_none() {
        // SAFETY: однопоточный момент старта, до инициализации tracing.
        unsafe { std::env::set_var("RUST_LOG", "debug") };
    }

    if has_valid_stderr() {
        // Хэндлы уже есть (пайп/файл/ConPTY) — перенаправлять ничего не нужно.
        println!("== Make It Sound Same: режим отладки ==");
        return true;
    }

    // SAFETY: консольные вызовы kernel32 по контракту; NULL-хэндлы stdio мы
    // заменяем на открытые CONIN$/CONOUT$ до любых записей в них.
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS).is_err() {
            // Родительской консоли нет (двойной клик) — создаём свою.
            let _ = AllocConsole();
        }
        if let Ok(conout) = CreateFileW(
            w!("CONOUT$"),
            GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        ) {
            let _ = SetStdHandle(STD_OUTPUT_HANDLE, conout);
            let _ = SetStdHandle(STD_ERROR_HANDLE, conout);
        }
        if let Ok(conin) = CreateFileW(
            w!("CONIN$"),
            GENERIC_READ.0,
            FILE_SHARE_READ,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        ) {
            let _ = SetStdHandle(STD_INPUT_HANDLE, conin);
        }
    }

    println!("== Make It Sound Same: режим отладки ==");
    println!(
        "уровень логов: {} (можно переопределить переменной RUST_LOG)",
        std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into())
    );
    true
}

/// Есть ли у процесса валидный stderr (у GUI-приложений без консоли GetStdHandle отдаёт NULL).
fn has_valid_stderr() -> bool {
    match unsafe { GetStdHandle(STD_ERROR_HANDLE) } {
        Ok(handle) => !handle.is_invalid(),
        Err(_) => false,
    }
}
