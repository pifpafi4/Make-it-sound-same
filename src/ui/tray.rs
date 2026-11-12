//! Иконка в трее. Живёт в отдельном потоке с собственным message pump
//! (Windows требует прокачку сообщений для tray-окна).
//!
//! Команды меню уходят: «Открыть» → UI-канал, «Выход» → Shutdown аудиопотоку
//! + закрытие окна.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;
use std::time::Duration;

use eframe::egui;
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::audio::Command;

/// Сообщения трея → UI.
#[derive(Debug, Clone, Copy)]
pub enum TrayMsg {
    Show,
    Quit,
}

const ID_OPEN: &str = "miss-open";
const ID_QUIT: &str = "miss-quit";

/// Запустить трей в отдельном потоке.
pub fn spawn_tray(cmd_tx: Sender<Command>, ui_ctx: egui::Context) -> Receiver<TrayMsg> {
    let (tx, rx) = channel();
    thread::Builder::new()
        .name("miss-tray".into())
        .spawn(move || {
            if let Err(e) = tray_loop(cmd_tx, ui_ctx, tx) {
                tracing::error!("трей остановился: {e:#}");
            }
        })
        .expect("запуск потока трея");
    rx
}

fn tray_loop(
    cmd_tx: Sender<Command>,
    ui_ctx: egui::Context,
    tx: Sender<TrayMsg>,
) -> anyhow::Result<()> {
    let menu = Menu::new();
    let open_item = MenuItem::with_id(ID_OPEN, "Открыть", true, None);
    let quit_item = MenuItem::with_id(ID_QUIT, "Выход", true, None);
    menu.append_items(&[&open_item, &quit_item])?;

    // Держим до конца потока: drop(tray) убирает иконку.
    let _tray: TrayIcon = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Make It Sound Same")
        .with_icon(app_icon_tray())
        .with_menu_on_left_click(true)
        .build()?;

    loop {
        // Windows: tray-окно живёт, только пока качаются сообщения.
        unsafe { pump_messages() };
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id.0 == ID_OPEN {
                // Когда окно скрыто, egui-цикл не крутится и TrayMsg никто не
                // прочитает — показываем окно напрямую через Win32.
                show_app_window();
                let _ = tx.send(TrayMsg::Show);
                ui_ctx.request_repaint();
            } else if event.id.0 == ID_QUIT {
                let _ = cmd_tx.send(Command::Shutdown);
                let _ = tx.send(TrayMsg::Quit);
                ui_ctx.request_repaint();
                // Страховка: когда окно свёрнуто в трей, egui-цикл не
                // обрабатывает события — graceful-путь не сработает.
                // Через 1.5 с гарантированно завершаем процесс; если UI вышел
                // нормально раньше, до exit дело не дойдёт.
                std::thread::spawn(|| {
                    std::thread::sleep(Duration::from_millis(1500));
                    tracing::info!("принудительное завершение из трея");
                    std::process::exit(0);
                });
            }
        }
        thread::sleep(Duration::from_millis(60));
    }
}

/// Показать главное окно напрямую (работает даже когда egui-цикл спит,
/// потому что окно скрыто). Окно ищем перечислением окон нашего процесса:
/// FindWindow по заголовку в PowerShell-тесте капризничал, а PID — надёжен.
fn show_app_window() {
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextW, GetWindowThreadProcessId, SetForegroundWindow, ShowWindow,
        SW_SHOW,
    };
    use windows::core::BOOL;
    use windows::Win32::Foundation::LPARAM;

    static FOUND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
    unsafe extern "system" fn enum_proc(hwnd: windows::Win32::Foundation::HWND, lparam: LPARAM) -> BOOL {
        let lparam = lparam.0 as usize;
        let mut pid = 0u32;
        // SAFETY: hwnd валиден (пришёл из EnumWindows), указатель на AtomicIsize жив.
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid == std::process::id() {
                let mut buf = [0u16; 64];
                let n = GetWindowTextW(hwnd, &mut buf);
                let title = String::from_utf16_lossy(&buf[..n.max(0) as usize]);
                if title == "Make It Sound Same" {
                    let slot = &*(lparam as *const std::sync::atomic::AtomicIsize);
                    slot.store(hwnd.0 as isize, std::sync::atomic::Ordering::SeqCst);
                    return BOOL(0); // нашли — прекращаем перечисление
                }
            }
        }
        BOOL(1)
    }

    // SAFETY: колбэк использует только валидные данные.
    unsafe {
        let found = &FOUND as *const std::sync::atomic::AtomicIsize as usize;
        let _ = EnumWindows(Some(enum_proc), LPARAM(found as isize));
        let handle = FOUND.load(std::sync::atomic::Ordering::SeqCst);
        if handle != 0 {
            let hwnd = windows::Win32::Foundation::HWND(handle as *mut core::ffi::c_void);
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
        } else {
            tracing::warn!("окно для показа из трея не найдено");
        }
    }
}
/// Прокачать все ожидающие сообщения оконного потока.
unsafe fn pump_messages() {
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, MSG, PeekMessageW, PM_REMOVE, TranslateMessage,
    };
    let mut msg = MSG::default();
    // SAFETY: msg инициализирован; PeekMessageW/Translate/Dispatch — стандартный pump.
    while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() } {
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Пиксели иконки 32×32: три «ползунка громкости» на прозрачном фоне.
pub fn app_icon_rgba() -> Vec<u8> {
    const W: usize = 32;
    const H: usize = 32;
    let mut rgba = vec![0u8; W * H * 4];
    let bars: [(usize, usize, usize); 3] = [(6, 12, 10), (13, 4, 18), (20, 0, 26)]; // (x, y_from, height)
    let accent = [30u8, 120, 215];
    for (x, y_from, height) in bars {
        for y in y_from..y_from + height {
            // Скругление углов ползунка: выкидываем пиксели по углам.
            let top = y == y_from || y == y_from + height - 1;
            let left_edge = x == bars[0].0 || x == bars[1].0 || x == bars[2].0;
            let _ = left_edge;
            let idx = (y * W + x) * 4;
            let rounded = top && (x % 7 == 0 || x % 7 == 6);
            if rounded {
                continue;
            }
            rgba[idx] = accent[0];
            rgba[idx + 1] = accent[1];
            rgba[idx + 2] = accent[2];
            rgba[idx + 3] = 255;
        }
    }
    rgba
}

fn app_icon_tray() -> Icon {
    Icon::from_rgba(app_icon_rgba(), 32, 32).expect("валидная RGBA-иконка")
}

/// Иконка окна (egui): 32×32 RGBA.
pub fn app_icon_egui() -> egui::viewport::IconData {
    egui::viewport::IconData {
        width: 32,
        height: 32,
        rgba: app_icon_rgba(),
    }
}
