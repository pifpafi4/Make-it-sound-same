//! Интерфейс: окно на egui (сессии / известные приложения / настройки / v2)
//! и иконка в трее (`tray`).

pub mod tray;

use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use eframe::egui;
use egui::{RichText, ViewportCommand};

use crate::audio::{AudioEvent, Command, KnownAppView, SessionKey, SessionView};
use crate::config::Config;

/// Запуск приложения: логирование, конфиг, аудиопоток, трей, окно.
/// `debug` — режим `--debug`: логи дублируются в консоль.
pub fn run_app(debug: bool) -> anyhow::Result<()> {
    let paths = crate::config::paths()?;
    crate::logging::init(paths.log.clone(), debug)?;
    tracing::info!("Make It Sound Same запускается");

    let config = crate::config::load_config(&paths.config);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([740.0, 660.0])
            .with_min_inner_size([560.0, 460.0])
            .with_icon(tray::app_icon_egui()),
        ..Default::default()
    };

    eframe::run_native(
        "Make It Sound Same",
        options,
        Box::new(move |cc| {
            setup_style(&cc.egui_ctx);
            let (cmd_tx, events) =
                crate::audio::spawn(cc.egui_ctx.clone(), config.clone())
                    .map_err(|e| anyhow::anyhow!("запуск аудиопотока: {e:#}"))?;
            let tray_rx = tray::spawn_tray(cmd_tx.clone(), cc.egui_ctx.clone());
            if config.start_minimized {
                cc.egui_ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            }
            Ok(Box::new(App::new(cmd_tx, events, tray_rx, config, cc.egui_ctx.clone())))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))?;
    Ok(())
}

/// Шрифты и плотность интерфейса: нативный Segoe UI (если найден в системе)
/// + размеры крупнее дефолтных (дефолт egui: Body/Button 14 — на Windows мелковато).
fn setup_style(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    // egui не подгружает системные шрифты сам: регистрируем Segoe UI из
    // C:\Windows\Fonts. Нет файла — остаётся дефолтный шрифт egui (с кириллицей).
    if let Ok(bytes) = std::fs::read("C:/Windows/Fonts/segoeui.ttf") {
        fonts
            .font_data
            .insert("segoe_ui".to_owned(), egui::FontData::from_owned(bytes).into());
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "segoe_ui".to_owned());
        ctx.set_fonts(fonts);
    } else {
        tracing::warn!("Segoe UI не найден, использую шрифт egui по умолчанию");
    }

    let mut style = (*ctx.style()).clone();
    style.text_styles = [
        (egui::TextStyle::Small, egui::FontId::proportional(14.0)),
        (egui::TextStyle::Body, egui::FontId::proportional(17.0)),
        (egui::TextStyle::Button, egui::FontId::proportional(17.0)),
        (egui::TextStyle::Heading, egui::FontId::proportional(24.0)),
        (egui::TextStyle::Monospace, egui::FontId::monospace(16.0)),
    ]
    .into();
    // Чуть больше воздуха между элементами.
    style.spacing.item_spacing = egui::vec2(10.0, 10.0);
    ctx.set_style(style);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Apps,
    Known,
    Settings,
    Loudness,
}

struct App {
    cmd_tx: Sender<Command>,
    events: Receiver<AudioEvent>,
    tray_rx: Receiver<tray::TrayMsg>,
    egui_ctx: egui::Context,
    config: Config,
    sessions: BTreeMap<SessionKey, SessionView>,
    known: Vec<KnownAppView>,
    device_name: String,
    fatal: Option<String>,
    tab: Tab,
    force_quit: bool,
    /// Отложенный текст с задержками re-assert (пока поле не валидно, не применяем).
    delays_text: String,
    /// Счётчик кадров (диагностика heartbeat).
    heartbeat_frames: u64,
    last_heartbeat: std::time::Instant,
    /// Идёт калибровка комфортного уровня (дедлайн).
    calibrating_until: Option<std::time::Instant>,
    /// Сообщение о результате калибровки (текст, показывать до).
    calibration_message: Option<(String, std::time::Instant)>,
}

impl App {
    fn new(
        cmd_tx: Sender<Command>,
        events: Receiver<AudioEvent>,
        tray_rx: Receiver<tray::TrayMsg>,
        config: Config,
        egui_ctx: egui::Context,
    ) -> Self {
        let delays_text = config
            .re_assert_delays_ms
            .iter()
            .map(|d| d.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        Self {
            cmd_tx,
            events,
            tray_rx,
            egui_ctx,
            config,
            sessions: BTreeMap::new(),
            known: Vec::new(),
            device_name: String::new(),
            fatal: None,
            tab: Tab::Apps,
            force_quit: false,
            delays_text,
            heartbeat_frames: 0,
            last_heartbeat: std::time::Instant::now(),
            calibrating_until: None,
            calibration_message: None,
        }
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                AudioEvent::SessionUpserted(view) => {
                    self.sessions.insert(view.key.clone(), *view);
                }
                AudioEvent::SessionRemoved(key) => {
                    self.sessions.remove(&key);
                }
                AudioEvent::KnownAppsChanged(list) => {
                    self.known = list;
                    self.known.sort_by(|a, b| a.display_name.cmp(&b.display_name));
                }
                AudioEvent::DeviceChanged(name) => self.device_name = name,
                AudioEvent::CalibrationDone(result) => {
                    self.calibrating_until = None;
                    let message = match result {
                        Some(reference) => format!(
                            "Опора: {reference:.1} dBFS — громче «опора + потолок» будет приглушаться"
                        ),
                        None => "Во время калибровки не было звука — включите музыку и попробуйте ещё раз".to_string(),
                    };
                    self.calibration_message = Some((
                        message,
                        std::time::Instant::now() + std::time::Duration::from_secs(15),
                    ));
                }
                AudioEvent::ConfigSynced(config) => {
                    self.config = *config;
                    self.delays_text = self
                        .config
                        .re_assert_delays_ms
                        .iter()
                        .map(|d| d.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                }
                AudioEvent::Fatal(message) => self.fatal = Some(message),
            }
        }
    }

    fn drain_tray(&mut self) {
        while let Ok(msg) = self.tray_rx.try_recv() {
            match msg {
                tray::TrayMsg::Show => {
                    self.egui_ctx
                        .send_viewport_cmd(ViewportCommand::Visible(true));
                    self.egui_ctx.send_viewport_cmd(ViewportCommand::Focus);
                }
                tray::TrayMsg::Quit => {
                    self.force_quit = true;
                    self.egui_ctx.send_viewport_cmd(ViewportCommand::Close);
                }
            }
        }
    }

    /// Обновить конфиг локально и отправить в аудиопоток (он сохранит на диск).
    fn update_config(&mut self, mutate: impl FnOnce(&mut Config)) {
        mutate(&mut self.config);
        let _ = self.cmd_tx.send(Command::UpdateConfig(Box::new(self.config.clone())));
    }

    /// Итоговый масштаб интерфейса = масштаб ОС × ui_scale из конфига.
    /// Вызывается каждый кадр: переживает смену монитора/DPI и применяется
    /// мгновенно при движении слайдера.
    fn sync_scale(&self, ctx: &egui::Context) {
        let native = ctx
            .input(|i| i.viewport().native_pixels_per_point)
            .unwrap_or(1.0);
        let target = native * self.config.ui_scale.clamp(0.5, 3.0);
        if (ctx.pixels_per_point() - target).abs() > 0.001 {
            ctx.set_pixels_per_point(target);
        }
    }

    /// Сменить масштаб из UI и применить сразу, не дожидаясь эха конфига.
    fn apply_ui_scale(&mut self, ctx: &egui::Context, scale: f32) {
        self.update_config(|c| c.ui_scale = scale);
        self.sync_scale(ctx);
    }

    fn ui_apps(&mut self, ui: &mut egui::Ui) {
        let exclude_system = self.config.exclude_system_sounds;
        let visible: Vec<SessionView> = self
            .sessions
            .values()
            .filter(|s| !(exclude_system && s.is_system))
            .cloned()
            .collect();

        if visible.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(24.0);
                ui.label("Пока ни одно приложение не играет звук.");
                ui.weak("Запустите что-нибудь — сессия появится здесь.");
            });
            return;
        }

        egui::ScrollArea::vertical().show(ui, |ui| {
            for session in visible {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(&session.display_name).strong());
                    self.status_badge(ui, &session);
                    if session.ducked {
                        ui.label(RichText::new("приглушено").small().color(egui::Color32::from_rgb(230, 150, 40)));
                    }
                    if !session.controllable {
                        ui.label(RichText::new("⚠ недоступно (админ?)").color(egui::Color32::from_rgb(220, 160, 40)).small());
                    }
                    if !session.active {
                        ui.label(RichText::new("тихо").weak().small());
                    }
                });
                if let Some(path) = &session.exe_path {
                    ui.label(RichText::new(path).weak().small());
                }
                if let Some(level) = session.level_dbfs {
                    self.level_bar(ui, level);
                }
                ui.add_enabled_ui(session.controllable, |ui| {
                    ui.horizontal(|ui| {
                        let mut volume = session.volume;
                        let slider = egui::Slider::new(&mut volume, 0.0..=1.0)
                            .custom_formatter(|v, _| format!("{}%", (v * 100.0).round() as i32))
                            .text("громкость");
                        if ui.add(slider).changed() {
                            if let Some(s) = self.sessions.get_mut(&session.key) {
                                s.volume = volume;
                            }
                            let _ = self.cmd_tx.send(Command::SetVolume {
                                key: session.key.clone(),
                                volume,
                            });
                        }
                        let mut mute = session.mute;
                        if ui.checkbox(&mut mute, "тихо").changed() {
                            if let Some(s) = self.sessions.get_mut(&session.key) {
                                s.mute = mute;
                            }
                            let _ = self.cmd_tx.send(Command::SetMute {
                                key: session.key.clone(),
                                mute,
                            });
                        }
                    });
                });
                ui.add_space(6.0);
                ui.separator();
                ui.add_space(2.0);
            }
        });
    }

    fn status_badge(&self, ui: &mut egui::Ui, session: &SessionView) {
        let (text, color) = match session.known {
            crate::audio::KnownState::Trusted => ("доверенное", egui::Color32::from_rgb(90, 180, 90)),
            crate::audio::KnownState::Defaulted => ("контролируется", egui::Color32::from_rgb(80, 140, 220)),
            crate::audio::KnownState::System => ("системные", egui::Color32::GRAY),
            crate::audio::KnownState::Unidentified => ("не опознан", egui::Color32::from_rgb(200, 120, 60)),
        };
        ui.label(RichText::new(text).small().color(color));
    }

    /// Полоска громкости сессии: 0 dBFS слева, −60 dBFS справа-в-ноль.
    fn level_bar(&self, ui: &mut egui::Ui, level_dbfs: f32) {
        let ceiling = self.config.loudness_reference_dbfs.unwrap_or(-20.0)
            + self.config.loudness_ceiling_offset_db;
        let frac = ((1.0 - level_dbfs / -60.0).clamp(0.0, 1.0) * 100.0).round();
        let color = if level_dbfs > ceiling {
            egui::Color32::from_rgb(230, 150, 40)
        } else {
            egui::Color32::from_rgb(90, 180, 90)
        };
        ui.horizontal(|ui| {
            let (rect, _) = ui
                .allocate_exact_size(egui::vec2(ui.available_width(), 6.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, 3.0, egui::Color32::from_gray(45));
            let fill = egui::Rect::from_min_size(
                rect.min,
                egui::vec2(rect.width() * frac as f32 / 100.0, rect.height()),
            );
            ui.painter().rect_filled(fill, 3.0, color);
            ui.label(
                RichText::new(format!("{:.0} dBFS", level_dbfs))
                    .small()
                    .weak(),
            );
        });
    }

    fn ui_known(&mut self, ui: &mut egui::Ui) {
        if self.known.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(24.0);
                ui.label("База известных приложений пуста.");
                ui.weak("Появится, как только утилита увидит первое приложение.");
            });
            return;
        }

        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.label(RichText::new(
                "Дефолт (5% и т.п.) применяется один раз — при первом появлении приложения. \
                 «Доверенное» — громкость никогда не трогается автоматически. \
                 «Забыть» — приложение снова считается новым (дефолт применится и к играющим сессиям).",
            ).weak().small());
            ui.add_space(8.0);
            for app in self.known.clone() {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(&app.display_name).strong());
                    if app.running {
                        ui.label(RichText::new("играет").small().color(egui::Color32::from_rgb(90, 180, 90)));
                    }
                    if app.trusted {
                        ui.label(RichText::new("доверенное").small().color(egui::Color32::from_rgb(90, 180, 90)));
                    } else if app.default_applied {
                        ui.label(RichText::new("дефолт применён").small().color(egui::Color32::from_rgb(80, 140, 220)));
                    }
                });
                ui.label(RichText::new(&app.exe_path).weak().small());
                ui.horizontal(|ui| {
                    let mut trusted = app.trusted;
                    if ui.checkbox(&mut trusted, "доверенное").changed() {
                        let _ = self.cmd_tx.send(Command::TrustApp {
                            exe_path: app.exe_path.clone(),
                            trusted,
                        });
                    }
                    if ui.button("Забыть").clicked() {
                        let _ = self.cmd_tx.send(Command::ForgetApp {
                            exe_path: app.exe_path.clone(),
                        });
                    }
                });
                ui.add_space(6.0);
                ui.separator();
            }
        });
    }

    fn ui_settings(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("settings").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("Масштаб интерфейса:");
            ui.horizontal_wrapped(|ui| {
                let mut scale = self.config.ui_scale;
                let slider = egui::Slider::new(&mut scale, 0.75..=2.0)
                    .step_by(0.05)
                    .custom_formatter(|v, _| format!("{}%", (v * 100.0).round() as i32));
                if ui.add(slider).changed() {
                    self.apply_ui_scale(ui.ctx(), scale);
                }
                for preset in [1.0f32, 1.25, 1.5, 1.75, 2.0] {
                    let label = format!("{}%", (preset * 100.0) as i32);
                    if ui
                        .add(
                            egui::Button::new(RichText::new(label).small())
                                .selected((self.config.ui_scale - preset).abs() < 0.01),
                        )
                        .clicked()
                    {
                        self.apply_ui_scale(ui.ctx(), preset);
                    }
                }
            });
            ui.end_row();

            ui.label("Громкость новых приложений:");
            let mut vol = self.config.unknown_app_volume;
            if ui
                .add(
                    egui::Slider::new(&mut vol, 0.0..=1.0)
                        .custom_formatter(|v, _| format!("{}%", (v * 100.0).round() as i32)),
                )
                .changed()
            {
                self.update_config(|c| c.unknown_app_volume = vol);
            }
            ui.end_row();

            ui.label("Повторы дефолта (мс):");
            let mut delays = self.delays_text.clone();
            let response = ui.add(
                egui::TextEdit::singleline(&mut delays)
                    .desired_width(160.0)
                    .hint_text("300, 1000, 2000"),
            );
            if response.lost_focus() {
                let parsed: Vec<u64> = delays
                    .split(',')
                    .filter_map(|s| s.trim().parse::<u64>().ok())
                    .filter(|d| *d > 0 && *d <= 60_000)
                    .collect();
                self.delays_text = delays.clone();
                self.update_config(|c| c.re_assert_delays_ms = parsed);
            }
            ui.end_row();

            ui.label("Системные звуки:");
            let mut excl = self.config.exclude_system_sounds;
            if ui.checkbox(&mut excl, "скрывать из списка приложений").changed() {
                self.update_config(|c| c.exclude_system_sounds = excl);
            }
            ui.end_row();

            ui.label("Автозапуск с Windows:");
            let mut auto = self.config.autostart;
            if ui.checkbox(&mut auto, "").changed() {
                self.update_config(|c| c.autostart = auto);
            }
            ui.end_row();

            ui.label("Стартовать свёрнутым в трей:");
            let mut minimized = self.config.start_minimized;
            if ui.checkbox(&mut minimized, "").changed() {
                self.update_config(|c| c.start_minimized = minimized);
            }
            ui.end_row();

            ui.label("Кнопка ✕ окна:");
            let mut close_to_tray = self.config.close_to_tray;
            if ui.checkbox(&mut close_to_tray, "сворачивать в трей").changed() {
                self.update_config(|c| c.close_to_tray = close_to_tray);
            }
            ui.end_row();
        });

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(4.0);
        self.ui_loudness_settings(ui);

        ui.add_space(16.0);
        ui.horizontal(|ui| {
            if ui.button("Открыть папку конфигурации").clicked() {
                if let Ok(dir) = crate::config::paths().map(|p| p.dir) {
                    let _ = std::process::Command::new("explorer").arg(dir).spawn();
                }
            }
            if ui.button("Сбросить все известные приложения").clicked() {
                for app in self.known.clone() {
                    let _ = self.cmd_tx.send(Command::ForgetApp {
                        exe_path: app.exe_path.clone(),
                    });
                }
            }
        });
        if let Some(log) = crate::logging::log_path() {
            ui.weak(format!("Лог: {}", log.display()));
        }
    }

    /// Секция «Автоприглушение громкости» (v2) в настройках.
    /// Калибровка доступна всегда — даже при выключенном приглушении:
    /// типичный сценарий «выключил лимитер → поднял громкость → перекалибровался».
    fn ui_loudness_settings(&mut self, ui: &mut egui::Ui) {
        ui.heading(RichText::new("Автоприглушение громкости").size(18.0));
        ui.label(RichText::new(
            "Приложения, играющие громче комфортного уровня + потолок, плавно приглушаются. \
             «Доверенные» и системные звуки не трогаются. Калибровать можно и при выключенном \
             приглушении — опора сохранится на потом.",
        ).weak().small());

        ui.add_space(4.0);
        let mut enabled = self.config.loudness_enabled;
        if ui.checkbox(&mut enabled, "включить автоприглушение").changed() {
            self.update_config(|c| c.loudness_enabled = enabled);
        }

        ui.add_space(4.0);
        egui::Grid::new("loudness_settings")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                ui.label("Потолок над комфортным уровнем:");
                let mut ceiling = self.config.loudness_ceiling_offset_db;
                if ui
                    .add(
                        egui::Slider::new(&mut ceiling, 2.0..=15.0)
                            .step_by(1.0)
                            .suffix(" дБ"),
                    )
                    .changed()
                {
                    self.update_config(|c| c.loudness_ceiling_offset_db = ceiling);
                }
                ui.end_row();

                ui.label("Комфортный уровень (опора):");
                ui.horizontal(|ui| {
                    match self.config.loudness_reference_dbfs {
                        Some(reference) => ui.label(format!("{reference:.1} dBFS")),
                        None => ui.label(RichText::new("не задан").weak()),
                    };
                    let calibrating = self.calibrating_until.is_some();
                    let label = if calibrating {
                        "Слушайте комфортный контент…"
                    } else {
                        "Калибровать…"
                    };
                    let button = ui.add_enabled(!calibrating, egui::Button::new(label));
                    if button.clicked() {
                        let _ = self.cmd_tx.send(Command::CalibrateLoudness);
                        self.calibrating_until =
                            Some(std::time::Instant::now() + std::time::Duration::from_secs(5));
                    }
                });
                ui.end_row();
            });

        if let Some(until) = self.calibrating_until {
            let left = until.saturating_duration_since(std::time::Instant::now()).as_secs() + 1;
            ui.label(RichText::new(format!("Измеряю… {left} с")).strong());
            if std::time::Instant::now() >= until {
                self.calibrating_until = None;
            }
        }
        if let Some((message, until)) = &self.calibration_message {
            ui.label(RichText::new(message).strong());
            if std::time::Instant::now() > *until {
                self.calibration_message = None;
            }
        }
    }

    fn ui_loudness(&mut self, ui: &mut egui::Ui) {
        ui.heading("Автоприглушение громкости");
        ui.add_space(8.0);
        ui.label(RichText::new(
            "Физические dB SPL без микрофона измерить нельзя, поэтому используется относительная шкала:\n\
             1. Вы слушаете что-то комфортное и нажимаете «Калибровать» (Настройки) — утилита запоминает \
             опорный уровень (dBFS).\n\
             2. Сессии, громче опоры + потолка (по умолчанию 5 дБ), плавно приглушаются до потолка.\n\
             3. Громкость возвращается к вашей, когда контент снова становится тише (с гистерезисом 2 дБ).\n\
             4. «Доверенные» приложения и системные звуки не трогаются.",
        ));
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(8.0);
        let reference = if let Some(reference) = self.config.loudness_reference_dbfs {
            format!("Опора: {:.1} dBFS, потолок: {:.1} dBFS", reference, reference + self.config.loudness_ceiling_offset_db)
        } else {
            "Опора не задана — откалибруйтесь в настройках".to_string()
        };
        ui.label(RichText::new(reference).strong());
        ui.add_space(8.0);
        ui.weak("Управление — на вкладке «Настройки». Полоски уровня — во вкладке «Приложения».");
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Держим цикл живым даже без ввода: так события трея и аудио
        // обрабатываются, когда окно свёрнуто в трей (Visible(false)).
        ctx.request_repaint_after(Duration::from_millis(500));
        self.heartbeat_frames += 1;
        let now = std::time::Instant::now();
        if now - self.last_heartbeat >= std::time::Duration::from_secs(2) {
            self.last_heartbeat = now;
            tracing::debug!("ui heartbeat: кадр #{}", self.heartbeat_frames);
        }
        self.drain_events();
        self.drain_tray();
        self.sync_scale(ctx);

        // Закрытие окна: либо прячем в трей, либо выходим.
        if ctx.input(|i| i.viewport().close_requested()) {
            if self.config.close_to_tray && !self.force_quit {
                ctx.send_viewport_cmd(ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            }
        }

        if let Some(message) = &self.fatal {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.heading("Аудиопоток остановился с ошибкой");
                ui.monospace(message);
                ui.weak("Смотрите лог. Перезапустите приложение.");
            });
            return;
        }

        // Верхняя панель: вкладки + устройство.
        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Apps, "Приложения");
                ui.selectable_value(&mut self.tab, Tab::Known, "Известные");
                ui.selectable_value(&mut self.tab, Tab::Settings, "Настройки");
                ui.selectable_value(&mut self.tab, Tab::Loudness, "Громкость");
            });
            ui.separator();
        });

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.weak(format!("Устройство: {}", if self.device_name.is_empty() { "—" } else { &self.device_name }));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.weak(format!("{} сессий", self.sessions.len()));
                });
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Apps => self.ui_apps(ui),
            Tab::Known => self.ui_known(ui),
            Tab::Settings => self.ui_settings(ui),
            Tab::Loudness => self.ui_loudness(ui),
        });
    }
}
