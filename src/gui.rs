use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};
#[cfg(target_os = "linux")]
use std::{fs::OpenOptions, io::Write};

use anyhow::{Context, Error, Result, bail};
use eframe::egui::{self, FontDefinitions, FontFamily, FontId, RichText};
#[cfg(target_os = "linux")]
use gtk::glib::{self, ControlFlow};
#[cfg(target_os = "windows")]
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
#[cfg(any(target_os = "windows", target_os = "macos"))]
use tray_icon::TrayIcon;
#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
use tray_icon::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

use crate::branding;
use crate::config::AppConfig;
#[cfg(target_os = "windows")]
use crate::paths::AppPaths;
use crate::platform::run_elevated;
#[cfg(target_os = "linux")]
use crate::platform::spawn_detached;
#[cfg(target_os = "windows")]
use crate::platform::{
    apply_app_window_icon, close_app_window, hide_app_window, restore_app_window,
    update_windows_shortcuts_for_exe,
};
use crate::service;
use crate::state::ServiceState;

const APP_WINDOW_TITLE: &str = "Linux.do Accelerator";
const APP_ID: &str = "linuxdo-accelerator";

pub fn run(config_path: PathBuf) -> Result<()> {
    let native_options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_WINDOW_TITLE)
            .with_app_id(APP_ID)
            .with_icon(branding::icon_data(256))
            .with_inner_size([680.0, 420.0])
            .with_min_inner_size([660.0, 400.0])
            .with_max_inner_size([920.0, 680.0])
            .with_minimize_button(!cfg!(target_os = "linux"))
            .with_maximize_button(false)
            .with_resizable(true),
        ..Default::default()
    };

    eframe::run_native(
        "Linux.do Accelerator",
        native_options,
        Box::new(move |cc| Ok(Box::new(AcceleratorApp::new(config_path.clone(), cc)))),
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))
}

#[cfg(target_os = "linux")]
pub fn run_tray_shell(config_path: PathBuf) -> Result<()> {
    log_linux_tray_event(&format!(
        "tray-shell start config={}",
        config_path.display()
    ));
    gtk::init().context("failed to initialize GTK for tray shell")?;

    let menu = Menu::new();
    let show_item = MenuItem::with_id("tray-show", "打开窗口", true, None);
    let quit_item = MenuItem::with_id("tray-quit", "退出程序", true, None);
    menu.append_items(&[&show_item, &PredefinedMenuItem::separator(), &quit_item])?;

    let tray_icon = TrayIconBuilder::new()
        .with_id("linuxdo-accelerator-tray-shell")
        .with_menu(Box::new(menu))
        .with_tooltip("Linux.do Accelerator")
        .with_icon(tray_window_icon()?)
        .build()
        .context("failed to create Linux tray icon")?;
    tray_icon
        .set_visible(true)
        .context("failed to show Linux tray icon")?;
    log_linux_tray_event("tray-shell icon visible");

    let (event_tx, event_rx) = mpsc::channel();
    let show_id = show_item.id().clone();
    let quit_id = quit_item.id().clone();
    let config_for_timeout = config_path.clone();

    let _menu_handler = MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if event.id == show_id {
            let _ = event_tx.send(TrayCommand::Restore);
        } else if event.id == quit_id {
            let _ = event_tx.send(TrayCommand::Quit);
        }
    }));

    glib::timeout_add_local(Duration::from_millis(100), move || {
        while let Ok(command) = event_rx.try_recv() {
            match command {
                TrayCommand::Restore => {
                    log_linux_tray_event("tray-shell restore clicked");
                    let _ = tray_icon.set_visible(false);
                    let _ = spawn_linux_ui_process(&config_for_timeout);
                    gtk::main_quit();
                    return ControlFlow::Break;
                }
                TrayCommand::Quit => {
                    log_linux_tray_event("tray-shell quit clicked");
                    let _ = tray_icon.set_visible(false);
                    gtk::main_quit();
                    return ControlFlow::Break;
                }
            }
        }

        ControlFlow::Continue
    });

    gtk::main();
    log_linux_tray_event("tray-shell exit");
    Ok(())
}

struct AcceleratorApp {
    config_path: PathBuf,
    config: AppConfig,
    status: ServiceState,
    feedback: String,
    busy: bool,
    action_rx: Option<Receiver<Result<String, String>>>,
    pending_action: Option<GuiAction>,
    optimistic_running: Option<(bool, Instant)>,
    last_refresh: Instant,
    show_about: bool,
    show_config: bool,
    logo: egui::TextureHandle,
    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    tray: Option<TrayState>,
    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    tray_rx: Receiver<TrayCommand>,
    #[cfg(target_os = "linux")]
    hidden_to_tray: bool,
    #[cfg(target_os = "linux")]
    last_minimized: bool,
    #[cfg(target_os = "macos")]
    hidden_to_tray: bool,
    #[cfg(target_os = "macos")]
    last_minimized: bool,
    #[cfg(target_os = "windows")]
    window_handle: Option<isize>,
    #[cfg(target_os = "windows")]
    hidden_to_tray: bool,
    #[cfg(target_os = "windows")]
    last_minimized: bool,
}

#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
struct TrayState {
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    tray_icon: TrayIcon,
    #[cfg(target_os = "linux")]
    control_tx: mpsc::Sender<TrayVisibilityCommand>,
}

#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
enum TrayCommand {
    Restore,
    Quit,
}

#[cfg(target_os = "linux")]
enum TrayVisibilityCommand {
    Show,
    Hide,
    Quit,
}

impl AcceleratorApp {
    fn new(config_path: PathBuf, cc: &eframe::CreationContext<'_>) -> Self {
        install_fonts(&cc.egui_ctx);
        install_theme(&cc.egui_ctx);

        let config = AppConfig::load_or_create(&config_path).unwrap_or_default();
        let status = service::status(Some(config_path.clone())).unwrap_or_default();
        #[cfg(target_os = "windows")]
        schedule_windows_shortcut_icon_refresh(&config_path);
        let logo = cc.egui_ctx.load_texture(
            "linuxdo-logo",
            branding::logo_image(96),
            egui::TextureOptions::LINEAR,
        );
        #[cfg(target_os = "linux")]
        let (tray, tray_rx) = build_tray_state(&cc.egui_ctx, None);
        #[cfg(target_os = "macos")]
        let (tray, tray_rx) = build_tray_state(&cc.egui_ctx, None);
        #[cfg(target_os = "windows")]
        let window_handle = capture_native_window_handle(cc);
        #[cfg(target_os = "windows")]
        if let Some(hwnd) = window_handle {
            let _ = apply_app_window_icon(hwnd);
        }
        #[cfg(target_os = "windows")]
        let (tray, tray_rx) = build_tray_state(&cc.egui_ctx, window_handle);
        Self {
            config_path,
            config,
            status,
            feedback: String::new(),
            busy: false,
            action_rx: None,
            pending_action: None,
            optimistic_running: None,
            last_refresh: Instant::now() - Duration::from_secs(2),
            show_about: false,
            show_config: false,
            logo,
            #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
            tray,
            #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
            tray_rx,
            #[cfg(target_os = "linux")]
            hidden_to_tray: false,
            #[cfg(target_os = "linux")]
            last_minimized: false,
            #[cfg(target_os = "macos")]
            hidden_to_tray: false,
            #[cfg(target_os = "macos")]
            last_minimized: false,
            #[cfg(target_os = "windows")]
            window_handle,
            #[cfg(target_os = "windows")]
            hidden_to_tray: false,
            #[cfg(target_os = "windows")]
            last_minimized: false,
        }
    }

    fn refresh_status(&mut self) {
        if let Ok(status) = service::status(Some(self.config_path.clone())) {
            self.status = self.apply_optimistic_state(status);
        }
        if let Ok(config) = AppConfig::load_or_create(&self.config_path) {
            self.config = config;
        }
    }

    fn apply_optimistic_state(&mut self, mut status: ServiceState) -> ServiceState {
        if let Some((running, deadline)) = self.optimistic_running {
            if Instant::now() >= deadline {
                self.optimistic_running = None;
                return status;
            }

            if running && !status.running && status.last_error.is_none() {
                status.running = true;
                status.status_text = "加速中".to_string();
            }

            if !running && status.running && status.last_error.is_none() {
                status.running = false;
                status.pid = None;
                status.status_text = "已停止".to_string();
            }
        }

        status
    }

    fn trigger_action(&mut self, action: GuiAction) {
        if self.busy {
            return;
        }

        self.busy = true;
        self.feedback = match action {
            GuiAction::Start => "正在申请权限并启动加速...".to_string(),
            GuiAction::Stop => "正在停止加速...".to_string(),
        };

        let config_path = self.config_path.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result =
                execute_action(&config_path, action).map_err(|error| format_error_chain(&error));
            let _ = tx.send(result);
        });
        self.action_rx = Some(rx);
        self.pending_action = Some(action);
    }

    fn poll_action(&mut self) {
        if let Some(rx) = &self.action_rx {
            match rx.try_recv() {
                Ok(result) => {
                    self.busy = false;
                    match result {
                        Ok(message) => {
                            self.feedback = message;
                            let deadline = Instant::now() + Duration::from_secs(4);
                            match self.pending_action {
                                Some(GuiAction::Start) => {
                                    self.status.running = true;
                                    self.status.status_text = "加速中".to_string();
                                    self.status.last_error = None;
                                    self.optimistic_running = Some((true, deadline));
                                }
                                Some(GuiAction::Stop) => {
                                    self.status.running = false;
                                    self.status.status_text = "已停止".to_string();
                                    self.status.last_error = None;
                                    self.optimistic_running = Some((false, deadline));
                                }
                                None => {}
                            }
                        }
                        Err(message) => {
                            self.optimistic_running = None;
                            self.status.running = false;
                            self.status.pid = None;
                            self.status.status_text = "启动失败".to_string();
                            self.status.last_error = Some(message.clone());
                            self.feedback = format!("操作失败: {message}");
                        }
                    }
                    self.last_refresh = Instant::now() - Duration::from_secs(2);
                    self.action_rx = None;
                    self.pending_action = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.busy = false;
                    self.optimistic_running = None;
                    self.feedback = "后台任务意外中断".to_string();
                    self.action_rx = None;
                    self.pending_action = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
    }

    fn headline_status(&self) -> (&'static str, egui::Color32) {
        if self.busy {
            return ("处理中", egui::Color32::from_rgb(250, 196, 92));
        }
        if self.status.running {
            return ("已接管", egui::Color32::from_rgb(106, 220, 155));
        }
        if self.status.last_error.is_some() {
            return ("异常", egui::Color32::from_rgb(255, 120, 100));
        }
        ("待启动", egui::Color32::from_rgb(162, 173, 184))
    }

    fn status_message(&self) -> String {
        if !self.feedback.is_empty() {
            return self.feedback.clone();
        }
        self.status.status_text.clone()
    }

    #[cfg(target_os = "windows")]
    fn minimize_to_tray(&mut self, ctx: &egui::Context) {
        if let Some(tray) = &self.tray {
            let _ = tray.tray_icon.set_visible(true);
        }
        self.hidden_to_tray = true;
        self.last_minimized = false;
        self.feedback = if self.status.running {
            "已最小化到系统托盘，后台加速仍在继续".to_string()
        } else {
            "已最小化到系统托盘".to_string()
        };
        if let Some(hwnd) = self.window_handle {
            let _ = hide_app_window(hwnd);
        }
        ctx.request_repaint();
    }

    #[cfg(target_os = "linux")]
    fn minimize_to_tray(&mut self, ctx: &egui::Context) {
        match spawn_linux_tray_shell(&self.config_path) {
            Ok(()) => {
                self.hidden_to_tray = true;
                self.last_minimized = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Err(error) => {
                self.feedback = format!("托盘最小化失败: {error}");
                self.hidden_to_tray = false;
                self.last_minimized = false;
                ctx.request_repaint();
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn minimize_to_tray(&mut self, ctx: &egui::Context) {
        if let Some(tray) = &self.tray {
            let _ = tray.tray_icon.set_visible(true);
        }
        self.hidden_to_tray = true;
        self.last_minimized = false;
        self.feedback = if self.status.running {
            "已最小化到菜单栏，后台加速仍在继续".to_string()
        } else {
            "已最小化到菜单栏".to_string()
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        ctx.request_repaint();
    }

    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    fn minimize_to_tray(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
    }

    #[cfg(target_os = "windows")]
    fn restore_from_tray(&mut self, ctx: &egui::Context) {
        self.hidden_to_tray = false;
        self.last_minimized = true;
        if let Some(tray) = &self.tray {
            let _ = tray.tray_icon.set_visible(false);
        }
        if let Some(hwnd) = self.window_handle {
            let _ = restore_app_window(hwnd);
            let _ = apply_app_window_icon(hwnd);
        }
        ctx.request_repaint();
    }

    #[cfg(target_os = "linux")]
    fn restore_from_tray(&mut self, ctx: &egui::Context) {
        self.hidden_to_tray = false;
        self.last_minimized = true;
        if let Some(tray) = &self.tray {
            let _ = tray.control_tx.send(TrayVisibilityCommand::Hide);
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        ctx.request_repaint();
    }

    #[cfg(target_os = "macos")]
    fn restore_from_tray(&mut self, ctx: &egui::Context) {
        self.hidden_to_tray = false;
        self.last_minimized = true;
        if let Some(tray) = &self.tray {
            let _ = tray.tray_icon.set_visible(false);
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        ctx.request_repaint();
    }

    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    fn poll_tray_events(&mut self, ctx: &egui::Context) {
        while let Ok(command) = self.tray_rx.try_recv() {
            match command {
                TrayCommand::Restore => self.restore_from_tray(ctx),
                TrayCommand::Quit => {
                    #[cfg(target_os = "linux")]
                    if let Some(tray) = &self.tray {
                        let _ = tray.control_tx.send(TrayVisibilityCommand::Quit);
                    }
                    #[cfg(target_os = "windows")]
                    if let Some(hwnd) = self.window_handle {
                        let _ = close_app_window(hwnd);
                    }
                    #[cfg(target_os = "linux")]
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    #[cfg(target_os = "macos")]
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    fn sync_minimize_to_tray(&mut self, ctx: &egui::Context) {
        let minimized = ctx.input(|input| input.viewport().minimized.unwrap_or(false));
        if minimized && !self.last_minimized && !self.hidden_to_tray {
            self.minimize_to_tray(ctx);
        }
        self.last_minimized = minimized && !self.hidden_to_tray;
    }
}

impl eframe::App for AcceleratorApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
        {
            self.poll_tray_events(ctx);
            self.sync_minimize_to_tray(ctx);
        }

        self.poll_action();

        if self.last_refresh.elapsed() >= Duration::from_secs(1) {
            self.refresh_status();
            self.last_refresh = Instant::now();
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(10.0, 10.0);
            let bg = egui::Color32::from_rgb(15, 18, 22);
            egui::Frame::new()
                .fill(bg)
                .inner_margin(egui::Margin::same(12))
                .corner_radius(egui::CornerRadius::same(18))
                .show(ui, |ui| {
                    let (headline, accent) = self.headline_status();
                    ui.horizontal(|ui| {
                        ui.add(egui::Image::new((self.logo.id(), egui::vec2(38.0, 38.0))));
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new("Linux.do Accelerator")
                                        .font(FontId::proportional(17.0))
                                        .strong()
                                        .color(egui::Color32::from_rgb(247, 247, 243)),
                                );
                                ui.add_space(6.0);
                                egui::Frame::new()
                                    .fill(accent.linear_multiply(0.18))
                                    .stroke(egui::Stroke::new(1.0, accent.linear_multiply(0.75)))
                                    .inner_margin(egui::Margin::symmetric(8, 4))
                                    .corner_radius(egui::CornerRadius::same(255))
                                    .show(ui, |ui| {
                                        ui.label(
                                            RichText::new(headline)
                                                .font(FontId::proportional(10.5))
                                                .strong()
                                                .color(accent),
                                        );
                                    });
                            });
                            ui.label(
                                RichText::new("linux.do 本地加速 / CLI + Desktop")
                                    .font(FontId::proportional(11.5))
                                    .color(egui::Color32::from_rgb(157, 168, 177)),
                            );
                        });
                    });

                    ui.add_space(10.0);

                    ui.columns(2, |columns| {
                        columns[0].spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                        egui::Frame::new()
                            .fill(egui::Color32::from_rgb(22, 28, 34))
                            .stroke(egui::Stroke::new(
                                1.0,
                                egui::Color32::from_rgb(42, 52, 61),
                            ))
                            .inner_margin(egui::Margin::same(12))
                            .corner_radius(egui::CornerRadius::same(14))
                            .show(&mut columns[0], |ui| {
                                ui.label(
                                    RichText::new("操作")
                                        .font(FontId::proportional(11.5))
                                        .strong()
                                        .color(egui::Color32::from_rgb(245, 188, 86)),
                                );
                                ui.label(
                                    RichText::new(self.status_message())
                                        .font(FontId::proportional(13.0))
                                        .color(egui::Color32::from_rgb(235, 238, 240)),
                                );
                                ui.add_space(4.0);

                                let primary_label = if self.status.running {
                                    "停止加速"
                                } else {
                                    "开始加速"
                                };
                                if ui
                                    .add_enabled(
                                        !self.busy,
                                        egui::Button::new(
                                            RichText::new(primary_label)
                                                .font(FontId::proportional(13.0))
                                                .strong(),
                                        )
                                        .min_size(egui::vec2(ui.available_width(), 40.0)),
                                    )
                                    .clicked()
                                {
                                    let action = if self.status.running {
                                        GuiAction::Stop
                                    } else {
                                        GuiAction::Start
                                    };
                                    self.trigger_action(action);
                                }

                                ui.horizontal(|ui| {
                                    if ui
                                        .add_enabled(
                                            !self.busy,
                                            egui::Button::new("最小化")
                                                .min_size(egui::vec2(78.0, 28.0)),
                                        )
                                        .clicked()
                                    {
                                        self.minimize_to_tray(ctx);
                                    }
                                    if ui
                                        .add(
                                            egui::Button::new("设置")
                                                .min_size(egui::vec2(64.0, 28.0)),
                                        )
                                        .clicked()
                                    {
                                        self.show_config = true;
                                    }
                                    if ui
                                        .add(
                                            egui::Button::new("关于")
                                                .min_size(egui::vec2(56.0, 28.0)),
                                        )
                                        .clicked()
                                    {
                                        self.show_about = true;
                                    }
                                });
                            });

                        egui::Frame::new()
                            .fill(egui::Color32::from_rgb(20, 24, 30))
                            .stroke(egui::Stroke::new(
                                1.0,
                                egui::Color32::from_rgb(38, 46, 54),
                            ))
                            .inner_margin(egui::Margin::same(10))
                            .corner_radius(egui::CornerRadius::same(12))
                            .show(&mut columns[0], |ui| {
                                ui.label(
                                    RichText::new("错误详情")
                                        .font(FontId::proportional(11.0))
                                        .strong()
                                        .color(egui::Color32::from_rgb(154, 167, 177)),
                                );
                                ui.add_space(4.0);
                                let details = self.status.last_error.as_deref().unwrap_or(
                                    "当前没有错误。启动失败时会直接显示真实原因。",
                                );
                                egui::ScrollArea::vertical()
                                    .max_height(128.0)
                                    .auto_shrink([false, false])
                                    .show(ui, |ui| {
                                        ui.label(
                                            RichText::new(details)
                                                .font(FontId::proportional(11.5))
                                                .color(if self.status.last_error.is_some() {
                                                    egui::Color32::from_rgb(255, 124, 102)
                                                } else {
                                                    egui::Color32::from_rgb(198, 205, 211)
                                                }),
                                        );
                                    });
                            });

                        columns[1].spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                        egui::Frame::new()
                            .fill(egui::Color32::from_rgb(24, 23, 18))
                            .stroke(egui::Stroke::new(
                                1.0,
                                egui::Color32::from_rgb(70, 62, 40),
                            ))
                            .inner_margin(egui::Margin::same(12))
                            .corner_radius(egui::CornerRadius::same(14))
                            .show(&mut columns[1], |ui| {
                                ui.label(
                                    RichText::new("接管范围")
                                        .font(FontId::proportional(11.5))
                                        .strong()
                                        .color(egui::Color32::from_rgb(247, 191, 84)),
                                );
                                ui.horizontal(|ui| {
                                    compact_metric(
                                        ui,
                                        "域名",
                                        &self.config.proxy_domains.len().to_string(),
                                    );
                                    compact_metric(
                                        ui,
                                        "DoH",
                                        &self.config.doh_endpoints.len().to_string(),
                                    );
                                    compact_metric(
                                        ui,
                                        "证书",
                                        &self.config.certificate_domains.len().to_string(),
                                    );
                                });
                                ui.add_space(4.0);
                                compact_row(ui, "上游", &self.config.upstream);
                                compact_row(
                                    ui,
                                    "DoH",
                                    &self
                                        .config
                                        .doh_endpoints
                                        .first()
                                        .cloned()
                                        .unwrap_or_else(|| "未配置".to_string()),
                                );
                            });

                        egui::Frame::new()
                            .fill(egui::Color32::from_rgb(21, 26, 31))
                            .stroke(egui::Stroke::new(
                                1.0,
                                egui::Color32::from_rgb(38, 46, 54),
                            ))
                            .inner_margin(egui::Margin::same(12))
                            .corner_radius(egui::CornerRadius::same(14))
                            .show(&mut columns[1], |ui| {
                                ui.label(
                                    RichText::new("提示")
                                        .font(FontId::proportional(11.5))
                                        .strong()
                                        .color(egui::Color32::from_rgb(127, 205, 175)),
                                );
                                ui.add_space(4.0);
                                egui::ScrollArea::vertical()
                                    .max_height(128.0)
                                    .auto_shrink([false, false])
                                    .show(ui, |ui| {
                                        ui.label(
                                            RichText::new(
                                                "DoH、接管域名和证书 SAN 都放在同一个 linuxdo-accelerator.toml 里，安装后直接改这一份就行。",
                                            )
                                            .font(FontId::proportional(11.5))
                                            .color(egui::Color32::from_rgb(211, 216, 220)),
                                        );
                                        ui.add_space(4.0);
                                        ui.label(
                                            RichText::new(
                                                "重签根证书后，浏览器最好完全退出再打开一次。",
                                            )
                                            .font(FontId::proportional(11.0))
                                            .color(egui::Color32::from_rgb(157, 168, 177)),
                                        );
                                    });
                            });
                    });
                });
        });

        if self.show_config {
            egui::Window::new("设置")
                .collapsible(false)
                .resizable(true)
                .default_width(560.0)
                .default_height(260.0)
                .show(ctx, |ui| {
                    ui.label(RichText::new("当前使用单配置文件。").strong());
                    ui.add_space(8.0);
                    ui.label(format!("主配置: {}", self.config_path.display()));
                    ui.add_space(8.0);
                    ui.label(format!("接管域名数: {}", self.config.proxy_domains.len()));
                    ui.label(format!("DoH 数量: {}", self.config.doh_endpoints.len()));
                    ui.label(format!(
                        "证书 SAN 数量: {}",
                        self.config.certificate_domains.len()
                    ));
                    if ui.button("关闭").clicked() {
                        self.show_config = false;
                    }
                });
        }

        if self.show_about {
            egui::Window::new("关于")
                .collapsible(false)
                .resizable(true)
                .default_width(500.0)
                .default_height(300.0)
                .show(ctx, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        ui.add(egui::Image::new((self.logo.id(), egui::vec2(42.0, 42.0))));
                        ui.label(RichText::new("Linux.do Accelerator").strong());
                        ui.label("原生 Rust 桌面壳 + CLI");
                        ui.label("支持证书安装、hosts 接管和本地 80/443 监听");
                        ui.label("DoH 与子域支持列表统一放在 linuxdo-accelerator.toml");
                        ui.label("点击加速会触发管理员提权，后台启动守护进程。");
                        ui.label("如果系统拒绝提权或端口监听失败，界面会直接显示真实错误。");
                    });
                    if ui.button("关闭").clicked() {
                        self.show_about = false;
                    }
                });
        }

        ctx.request_repaint_after(Duration::from_millis(250));
    }
}

#[derive(Clone, Copy)]
enum GuiAction {
    Start,
    Stop,
}

fn execute_action(config_path: &Path, action: GuiAction) -> Result<String> {
    #[cfg(target_os = "macos")]
    if matches!(action, GuiAction::Start) {
        service::prepare_certificate(Some(config_path.to_path_buf()))
            .with_context(|| "macOS certificate preparation failed")?;
    }

    let cli_binary = locate_action_binary()?;
    let subcommand = match action {
        GuiAction::Start => "helper-start",
        GuiAction::Stop => "helper-stop",
    };
    let args = vec![
        "--config".to_string(),
        config_path.to_string_lossy().into_owned(),
        subcommand.to_string(),
    ];
    if let Err(error) = run_elevated(&cli_binary, &args) {
        if let Ok(status) = service::status(Some(config_path.to_path_buf()))
            && let Some(last_error) = status.last_error
        {
            return Err(Error::msg(last_error))
                .with_context(|| "elevation or command execution failed");
        }
        return Err(error).with_context(|| "elevation or command execution failed");
    }

    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        let status = service::status(Some(config_path.to_path_buf()))?;
        match action {
            GuiAction::Start if status.running => {
                return Ok("加速已启动，可以直接最小化窗口".to_string());
            }
            GuiAction::Stop if !status.running => {
                return Ok("加速已停止".to_string());
            }
            _ => {
                if let Some(error) = status.last_error {
                    bail!(error);
                }
                if Instant::now() >= deadline {
                    bail!(
                        "service state did not update in time (status: {})",
                        status.status_text
                    );
                }
                thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn spawn_linux_tray_shell(config_path: &Path) -> Result<()> {
    let gui_binary = locate_gui_binary()?;
    let args = vec![
        "--config".to_string(),
        config_path.to_string_lossy().into_owned(),
        "tray-shell".to_string(),
    ];
    log_linux_tray_event(&format!(
        "spawn tray-shell exe={} config={}",
        gui_binary.display(),
        config_path.display()
    ));
    spawn_detached(&gui_binary, &args).context("failed to start Linux tray shell")?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn spawn_linux_ui_process(config_path: &Path) -> Result<()> {
    let gui_binary = locate_gui_binary()?;
    let args = vec![
        "--config".to_string(),
        config_path.to_string_lossy().into_owned(),
        "gui".to_string(),
    ];
    log_linux_tray_event(&format!(
        "spawn ui exe={} config={}",
        gui_binary.display(),
        config_path.display()
    ));
    spawn_detached(&gui_binary, &args).context("failed to reopen Linux UI")?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn log_linux_tray_event(message: &str) {
    let path = std::env::temp_dir().join("linuxdo-tray.log");
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{message}");
    }
}

fn locate_action_binary() -> Result<PathBuf> {
    locate_current_or_sibling_binary(action_binary_name())
}

fn locate_gui_binary() -> Result<PathBuf> {
    locate_current_or_sibling_binary(gui_binary_name())
}

fn locate_current_or_sibling_binary(binary_name: &str) -> Result<PathBuf> {
    let current = std::env::current_exe().context("failed to locate current executable")?;
    if current
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == binary_name)
    {
        return Ok(current);
    }

    let sibling = current.with_file_name(binary_name);
    if sibling.exists() {
        return Ok(sibling);
    }

    bail!("failed to locate binary {}", sibling.display())
}

fn action_binary_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "linuxdo-accelerator.exe"
    } else {
        "linuxdo-accelerator"
    }
}

fn gui_binary_name() -> &'static str {
    action_binary_name()
}

fn format_error_chain(error: &Error) -> String {
    let mut lines = Vec::new();
    for cause in error.chain() {
        let message = cause.to_string();
        if lines.last() != Some(&message) {
            lines.push(message);
        }
    }
    lines.join("\ncaused by: ")
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        "linuxdo_cjk".to_string(),
        egui::FontData::from_static(include_bytes!("../assets/fonts/DroidSansFallbackFull.ttf"))
            .into(),
    );
    if let Some(family) = fonts.families.get_mut(&FontFamily::Proportional) {
        family.insert(0, "linuxdo_cjk".to_string());
    }
    if let Some(family) = fonts.families.get_mut(&FontFamily::Monospace) {
        family.insert(0, "linuxdo_cjk".to_string());
    }
    ctx.set_fonts(fonts);
}

fn install_theme(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.override_text_color = Some(egui::Color32::from_rgb(236, 239, 241));
    style.visuals.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(22, 25, 29);
    style.visuals.widgets.noninteractive.fg_stroke.color = egui::Color32::from_rgb(236, 239, 241);
    style.visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(39, 45, 53);
    style.visuals.widgets.inactive.weak_bg_fill = egui::Color32::from_rgb(39, 45, 53);
    style.visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(58, 69, 79);
    style.visuals.widgets.hovered.fg_stroke.color = egui::Color32::WHITE;
    style.visuals.widgets.active.bg_fill = egui::Color32::from_rgb(243, 180, 66);
    style.visuals.widgets.active.fg_stroke.color = egui::Color32::from_rgb(24, 24, 22);
    style.visuals.widgets.open.bg_fill = egui::Color32::from_rgb(44, 50, 58);
    style.visuals.selection.bg_fill = egui::Color32::from_rgb(243, 180, 66);
    style.visuals.selection.stroke.color = egui::Color32::from_rgb(24, 24, 22);
    style.visuals.window_fill = egui::Color32::from_rgb(14, 17, 21);
    style.visuals.window_stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(52, 59, 67));
    style.visuals.extreme_bg_color = egui::Color32::from_rgb(14, 17, 21);
    style.visuals.faint_bg_color = egui::Color32::from_rgb(20, 24, 28);
    style.visuals.window_corner_radius = egui::CornerRadius::same(18);
    style.visuals.menu_corner_radius = egui::CornerRadius::same(10);
    style.visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(10);
    style.visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(10);
    style.visuals.widgets.active.corner_radius = egui::CornerRadius::same(10);
    style.spacing.button_padding = egui::vec2(12.0, 8.0);
    style.text_styles.insert(
        egui::TextStyle::Button,
        FontId::new(12.0, FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Body,
        FontId::new(12.0, FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Small,
        FontId::new(11.0, FontFamily::Proportional),
    );
    ctx.set_style(style);
}

fn compact_metric(ui: &mut egui::Ui, label: &str, value: &str) {
    egui::Frame::new()
        .fill(egui::Color32::from_rgb(31, 31, 25))
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(66, 62, 42)))
        .inner_margin(egui::Margin::symmetric(8, 6))
        .corner_radius(egui::CornerRadius::same(10))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new(label)
                        .font(FontId::proportional(10.0))
                        .color(egui::Color32::from_rgb(150, 160, 169)),
                );
                ui.label(
                    RichText::new(value)
                        .font(FontId::proportional(15.0))
                        .strong()
                        .color(egui::Color32::from_rgb(246, 232, 192)),
                );
            });
        });
}

fn compact_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.add_sized(
            [42.0, 18.0],
            egui::Label::new(
                RichText::new(label)
                    .font(FontId::proportional(10.5))
                    .strong()
                    .color(egui::Color32::from_rgb(185, 191, 197)),
            ),
        );
        ui.label(
            RichText::new(value)
                .font(FontId::monospace(10.5))
                .color(egui::Color32::from_rgb(224, 227, 230)),
        );
    });
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn build_tray_state(
    ctx: &egui::Context,
    window_handle: Option<isize>,
) -> (Option<TrayState>, Receiver<TrayCommand>) {
    let (tx, rx) = mpsc::channel();
    let menu = Menu::new();
    let show_item = MenuItem::with_id("tray-show", "打开窗口", true, None);
    let quit_item = MenuItem::with_id("tray-quit", "退出程序", true, None);
    let tx_tray_click = tx.clone();
    let tx_menu = tx.clone();
    let ctx_tray_click = ctx.clone();
    let ctx_menu = ctx.clone();
    TrayIconEvent::set_event_handler(Some(move |event| match event {
        TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        }
        | TrayIconEvent::DoubleClick {
            button: MouseButton::Left,
            ..
        } => {
            let _ = tx_tray_click.send(TrayCommand::Restore);
            #[cfg(target_os = "windows")]
            if let Some(hwnd) = window_handle {
                let _ = restore_app_window(hwnd);
                let _ = apply_app_window_icon(hwnd);
            }
            ctx_tray_click.request_repaint();
        }
        _ => {}
    }));
    let show_id = show_item.id().clone();
    let quit_id = quit_item.id().clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if event.id == show_id {
            let _ = tx_menu.send(TrayCommand::Restore);
            #[cfg(target_os = "windows")]
            if let Some(hwnd) = window_handle {
                let _ = restore_app_window(hwnd);
                let _ = apply_app_window_icon(hwnd);
            }
            ctx_menu.request_repaint();
        } else if event.id == quit_id {
            let _ = tx_menu.send(TrayCommand::Quit);
            #[cfg(target_os = "windows")]
            if let Some(hwnd) = window_handle {
                let _ = close_app_window(hwnd);
            }
            ctx_menu.request_repaint();
        }
    }));

    let result = (|| -> Result<TrayState> {
        menu.append_items(&[&show_item, &PredefinedMenuItem::separator(), &quit_item])?;

        let tray_icon = TrayIconBuilder::new()
            .with_id("linuxdo-accelerator-tray")
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip("Linux.do Accelerator")
            .with_icon(tray_window_icon()?)
            .build()
            .context("failed to create desktop tray icon")?;
        tray_icon
            .set_visible(false)
            .context("failed to hide desktop tray icon")?;

        Ok(TrayState { tray_icon })
    })();

    (result.ok(), rx)
}

#[cfg(target_os = "linux")]
fn build_tray_state(
    ctx: &egui::Context,
    _window_handle: Option<isize>,
) -> (Option<TrayState>, Receiver<TrayCommand>) {
    let (event_tx, event_rx) = mpsc::channel();
    let (control_tx, control_rx) = mpsc::channel();
    let ctx_menu = ctx.clone();
    let ctx_tray = ctx.clone();

    thread::spawn(move || {
        if gtk::init().is_err() {
            return;
        }

        let menu = Menu::new();
        let show_item = MenuItem::with_id("tray-show", "打开窗口", true, None);
        let quit_item = MenuItem::with_id("tray-quit", "退出程序", true, None);
        if menu
            .append_items(&[&show_item, &PredefinedMenuItem::separator(), &quit_item])
            .is_err()
        {
            return;
        }

        let tray_icon = match tray_window_icon() {
            Ok(icon) => TrayIconBuilder::new()
                .with_id("linuxdo-accelerator-tray")
                .with_menu(Box::new(menu))
                .with_tooltip("Linux.do Accelerator")
                .with_icon(icon)
                .build()
                .ok(),
            Err(_) => None,
        };

        let Some(tray_icon) = tray_icon else {
            return;
        };
        let _ = tray_icon.set_visible(false);

        let show_id = show_item.id().clone();
        let quit_id = quit_item.id().clone();
        let event_tx_menu = event_tx.clone();
        let _menu_handler = MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            if event.id == show_id {
                let _ = event_tx_menu.send(TrayCommand::Restore);
                ctx_menu.request_repaint();
            } else if event.id == quit_id {
                let _ = event_tx_menu.send(TrayCommand::Quit);
                ctx_menu.request_repaint();
            }
        }));

        let event_tx_tray = event_tx.clone();
        let _tray_handler = TrayIconEvent::set_event_handler(Some(move |event| match event {
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }
            | TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            } => {
                let _ = event_tx_tray.send(TrayCommand::Restore);
                ctx_tray.request_repaint();
            }
            _ => {}
        }));

        glib::timeout_add_local(Duration::from_millis(100), move || {
            while let Ok(command) = control_rx.try_recv() {
                match command {
                    TrayVisibilityCommand::Show => {
                        let _ = tray_icon.set_visible(true);
                    }
                    TrayVisibilityCommand::Hide => {
                        let _ = tray_icon.set_visible(false);
                    }
                    TrayVisibilityCommand::Quit => {
                        let _ = tray_icon.set_visible(false);
                        gtk::main_quit();
                        return ControlFlow::Break;
                    }
                }
            }
            ControlFlow::Continue
        });

        gtk::main();
    });

    (Some(TrayState { control_tx }), event_rx)
}

#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
fn tray_window_icon() -> Result<tray_icon::Icon> {
    let icon = branding::icon_data(64);
    tray_icon::Icon::from_rgba(icon.rgba, icon.width, icon.height)
        .map_err(|error| anyhow::anyhow!(error.to_string()))
}

#[cfg(target_os = "windows")]
fn capture_native_window_handle(cc: &eframe::CreationContext<'_>) -> Option<isize> {
    match cc.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
        _ => None,
    }
}

#[cfg(target_os = "windows")]
fn schedule_windows_shortcut_icon_refresh(config_path: &Path) {
    let result = (|| -> Result<()> {
        let current_exe = std::env::current_exe().context("failed to locate current executable")?;
        let paths = AppPaths::resolve(Some(config_path.to_path_buf()))?;
        std::fs::create_dir_all(&paths.runtime_dir)
            .with_context(|| format!("failed to create {}", paths.runtime_dir.display()))?;

        let stamp_path = paths.runtime_dir.join("windows-shortcut-icon-sync.txt");
        let stamp = format!("{}\n{}", env!("CARGO_PKG_VERSION"), current_exe.display());
        if std::fs::read_to_string(&stamp_path).ok().as_deref() == Some(stamp.as_str()) {
            return Ok(());
        }

        thread::spawn(move || {
            if update_windows_shortcuts_for_exe(&current_exe).is_ok() {
                let _ = std::fs::write(&stamp_path, stamp);
            }
        });
        Ok(())
    })();

    let _ = result;
}

#[cfg(not(target_os = "windows"))]
fn schedule_windows_shortcut_icon_refresh(_config_path: &Path) {}
