use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
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
use crate::platform::spawn_detached;
#[cfg(target_os = "windows")]
use crate::platform::{apply_app_window_icon, update_windows_shortcuts_for_exe};
use crate::runtime_log::{append as append_runtime_log, read_recent_lines};
use crate::service;
use crate::state::ServiceState;

const APP_WINDOW_TITLE: &str = "Linux.do Accelerator";
const APP_ID: &str = "linuxdo-accelerator";
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const ACTIVE_REPAINT_INTERVAL: Duration = Duration::from_millis(100);
const IDLE_REPAINT_INTERVAL: Duration = Duration::from_secs(5);
const TRAY_REPAINT_INTERVAL: Duration = Duration::from_secs(15);
const EMBEDDED_CJK_FONT: &[u8] = include_bytes!("../assets/fonts/DroidSansFallbackFull.ttf");
const LAUNCHER_WINDOW_SIZE: [f32; 2] = [720.0, 260.0];
const DETAILS_WINDOW_SIZE: [f32; 2] = [760.0, 520.0];
const TITLE_BAR_HEIGHT: f32 = 52.0;

pub fn run(config_path: PathBuf) -> Result<()> {
    let native_options = eframe::NativeOptions {
        renderer: default_renderer(),
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_WINDOW_TITLE)
            .with_app_id(APP_ID)
            .with_icon(branding::icon_data(256))
            .with_decorations(false)
            .with_inner_size(LAUNCHER_WINDOW_SIZE)
            .with_min_inner_size(LAUNCHER_WINDOW_SIZE)
            .with_max_inner_size(LAUNCHER_WINDOW_SIZE)
            .with_minimize_button(true)
            .with_maximize_button(false)
            .with_resizable(false),
        ..Default::default()
    };

    eframe::run_native(
        "Linux.do Accelerator",
        native_options,
        Box::new(move |cc| Ok(Box::new(AcceleratorApp::new(config_path.clone(), cc)))),
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))
}

fn default_renderer() -> eframe::Renderer {
    eframe::Renderer::Glow
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
                    let _ = spawn_ui_process(&config_for_timeout);
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

#[cfg(any(target_os = "windows", target_os = "macos"))]
pub fn run_tray_shell(config_path: PathBuf) -> Result<()> {
    use winit::application::ApplicationHandler;
    use winit::event::StartCause;
    use winit::event_loop::{ActiveEventLoop, EventLoop};

    #[derive(Debug)]
    enum TrayShellEvent {
        Restore,
        Quit,
    }

    struct TrayShellApp {
        config_path: PathBuf,
        tray_icon: Option<TrayIcon>,
        show_item: MenuItem,
        quit_item: MenuItem,
    }

    impl TrayShellApp {
        fn create_tray_icon(&mut self) -> Result<()> {
            if self.tray_icon.is_some() {
                return Ok(());
            }

            let menu = Menu::new();
            menu.append_items(&[
                &self.show_item,
                &PredefinedMenuItem::separator(),
                &self.quit_item,
            ])?;

            let tray_icon = TrayIconBuilder::new()
                .with_id("linuxdo-accelerator-tray-shell")
                .with_menu(Box::new(menu))
                .with_menu_on_left_click(false)
                .with_tooltip("Linux.do Accelerator")
                .with_icon(tray_window_icon()?)
                .build()
                .context("failed to create tray shell icon")?;
            self.tray_icon = Some(tray_icon);
            Ok(())
        }
    }

    impl ApplicationHandler<TrayShellEvent> for TrayShellApp {
        fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

        fn window_event(
            &mut self,
            _event_loop: &ActiveEventLoop,
            _window_id: winit::window::WindowId,
            _event: winit::event::WindowEvent,
        ) {
        }

        fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
            if cause == StartCause::Init && self.create_tray_icon().is_err() {
                event_loop.exit();
            }
        }

        fn user_event(&mut self, event_loop: &ActiveEventLoop, event: TrayShellEvent) {
            match event {
                TrayShellEvent::Restore => {
                    let _ = spawn_ui_process(&self.config_path);
                    event_loop.exit();
                }
                TrayShellEvent::Quit => {
                    event_loop.exit();
                }
            }
        }
    }

    let show_item = MenuItem::with_id("tray-show", "打开窗口", true, None);
    let quit_item = MenuItem::with_id("tray-quit", "退出程序", true, None);
    let show_id = show_item.id().clone();
    let quit_id = quit_item.id().clone();

    let event_loop = EventLoop::<TrayShellEvent>::with_user_event()
        .build()
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;

    let proxy = event_loop.create_proxy();
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
            let _ = proxy.send_event(TrayShellEvent::Restore);
        }
        _ => {}
    }));

    let proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if event.id == show_id {
            let _ = proxy.send_event(TrayShellEvent::Restore);
        } else if event.id == quit_id {
            let _ = proxy.send_event(TrayShellEvent::Quit);
        }
    }));

    let mut app = TrayShellApp {
        config_path,
        tray_icon: None,
        show_item,
        quit_item,
    };
    let result = event_loop
        .run_app(&mut app)
        .map_err(|error| anyhow::anyhow!(error.to_string()));
    TrayIconEvent::set_event_handler::<fn(TrayIconEvent)>(None);
    MenuEvent::set_event_handler::<fn(MenuEvent)>(None);
    result
}

struct AcceleratorApp {
    config_path: PathBuf,
    config: AppConfig,
    status: ServiceState,
    recent_logs: Vec<String>,
    feedback: String,
    busy: bool,
    action_rx: Option<Receiver<Result<String, String>>>,
    pending_action: Option<GuiAction>,
    confirm_action: Option<GuiAction>,
    optimistic_running: Option<(bool, Instant)>,
    last_refresh: Instant,
    config_modified_at: Option<SystemTime>,
    runtime_log_modified_at: Option<SystemTime>,
    current_page: UiPage,
    logo: egui::TextureHandle,
    #[cfg(target_os = "linux")]
    tray: Option<TrayState>,
    #[cfg(target_os = "linux")]
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
    tray: Option<TrayState>,
    #[cfg(target_os = "windows")]
    tray_rx: Receiver<TrayCommand>,
    #[cfg(target_os = "windows")]
    window_handle: Option<isize>,
    #[cfg(target_os = "windows")]
    hidden_to_tray: bool,
    #[cfg(target_os = "windows")]
    last_minimized: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UiPage {
    Launcher,
    Details,
    Config,
    About,
}

#[cfg(target_os = "linux")]
struct TrayState {
    control_tx: mpsc::Sender<TrayVisibilityCommand>,
}

#[cfg(target_os = "windows")]
struct TrayState {
    tray_icon: tray_icon::TrayIcon,
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
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
        let config_modified_at = file_modified_at(&config_path);
        let status = service::status(Some(config_path.clone())).unwrap_or_default();
        let recent_logs = load_recent_runtime_logs(&config_path);
        let runtime_log_modified_at = runtime_log_file_modified_at(&config_path);
        #[cfg(target_os = "windows")]
        schedule_windows_shortcut_icon_refresh(&config_path);
        let logo = cc.egui_ctx.load_texture(
            "linuxdo-logo",
            branding::logo_image(96),
            egui::TextureOptions::LINEAR,
        );
        #[cfg(target_os = "linux")]
        let (tray, tray_rx) = build_tray_state(&cc.egui_ctx, None);
        #[cfg(target_os = "windows")]
        let window_handle = capture_native_window_handle(cc);
        #[cfg(target_os = "windows")]
        if let Some(hwnd) = window_handle {
            let _ = apply_app_window_icon(hwnd);
        }
        #[cfg(target_os = "windows")]
        let (tray, tray_rx) = build_windows_tray_state(&cc.egui_ctx);
        Self {
            config_path,
            config,
            status,
            recent_logs,
            feedback: String::new(),
            busy: false,
            action_rx: None,
            pending_action: None,
            confirm_action: None,
            optimistic_running: None,
            last_refresh: Instant::now() - Duration::from_secs(2),
            config_modified_at,
            runtime_log_modified_at,
            current_page: UiPage::Launcher,
            logo,
            #[cfg(target_os = "linux")]
            tray,
            #[cfg(target_os = "linux")]
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
            tray,
            #[cfg(target_os = "windows")]
            tray_rx,
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
        let current_log_modified_at = runtime_log_file_modified_at(&self.config_path);
        if current_log_modified_at != self.runtime_log_modified_at {
            self.recent_logs = load_recent_runtime_logs(&self.config_path);
            self.runtime_log_modified_at = current_log_modified_at;
        }
        let current_config_modified_at = file_modified_at(&self.config_path);
        if current_config_modified_at != self.config_modified_at {
            if let Ok(config) = AppConfig::load_or_create(&self.config_path) {
                self.config = config;
            }
            self.config_modified_at = current_config_modified_at;
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
        self.feedback = action.pending_message().to_string();

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
                            match self.pending_action {
                                Some(GuiAction::Start) => {
                                    self.status.running = false;
                                    self.status.pid = None;
                                    self.status.status_text = "启动失败".to_string();
                                    self.status.last_error = Some(message.clone());
                                }
                                _ => {
                                    self.refresh_status();
                                    self.status.last_error = Some(message.clone());
                                }
                            }
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

    fn recent_logs_or_placeholder(&self) -> Vec<String> {
        if self.recent_logs.is_empty() {
            vec!["暂无运行日志。执行开始、停止、恢复等操作后会在这里显示。".to_string()]
        } else {
            self.recent_logs.clone()
        }
    }

    fn render_window_title_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let width = ui.available_width();
        let inner = ui.allocate_ui_with_layout(
            egui::vec2(width, TITLE_BAR_HEIGHT),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(10.0, 0.0);
                ui.add_space(12.0);
                ui.add(egui::Image::new((self.logo.id(), egui::vec2(30.0, 30.0))));
                ui.label(
                    RichText::new(APP_WINDOW_TITLE)
                        .font(FontId::proportional(17.0))
                        .strong()
                        .color(egui::Color32::from_rgb(244, 245, 247)),
                );

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(6.0);
                    if ui
                        .add(title_bar_button("X", egui::vec2(38.0, 28.0), true, true))
                        .clicked()
                    {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    ui.add_space(2.0);
                    if ui
                        .add(title_bar_button("_", egui::vec2(38.0, 28.0), false, true))
                        .clicked()
                    {
                        self.minimize_to_tray(ctx);
                    }
                });
            },
        );

        // Enable drag on the title bar for window movement
        let title_bar_response =
            ui.interact(inner.response.rect, ui.id().with("title_bar_drag"), egui::Sense::drag());
        if title_bar_response.drag_started() {
            ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }

        ui.painter().line_segment(
            [
                inner.response.rect.left_bottom(),
                inner.response.rect.right_bottom(),
            ],
            egui::Stroke::new(1.0, egui::Color32::from_rgb(48, 53, 61)),
        );
    }

    fn render_brand_banner(&self, ui: &mut egui::Ui, title: &str, summary: &str) {
        let (headline, accent) = self.headline_status();
        egui::Frame::new()
            .fill(egui::Color32::from_rgb(22, 26, 32))
            .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(50, 56, 64)))
            .inner_margin(egui::Margin::symmetric(16, 14))
            .corner_radius(egui::CornerRadius::same(14))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add(egui::Image::new((self.logo.id(), egui::vec2(32.0, 32.0))));
                    ui.vertical(|ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                RichText::new(title)
                                    .font(FontId::proportional(17.0))
                                    .strong()
                                    .color(egui::Color32::from_rgb(244, 245, 247)),
                            );
                            ui.label(
                                RichText::new(format!("v{APP_VERSION}"))
                                    .font(FontId::proportional(10.5))
                                    .color(egui::Color32::from_rgb(140, 150, 160)),
                            );
                        });
                        ui.label(
                            RichText::new(summary)
                                .font(FontId::proportional(11.5))
                                .color(egui::Color32::from_rgb(155, 164, 172)),
                        );
                    });

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        egui::Frame::new()
                            .fill(accent.linear_multiply(0.14))
                            .stroke(egui::Stroke::new(1.0, accent.linear_multiply(0.5)))
                            .inner_margin(egui::Margin::symmetric(12, 5))
                            .corner_radius(egui::CornerRadius::same(255))
                            .show(ui, |ui| {
                                ui.label(
                                    RichText::new(headline)
                                        .font(FontId::proportional(11.0))
                                        .strong()
                                        .color(accent),
                                );
                            });
                    });
                });
            });
    }

    fn render_launcher_status_card(&self, ui: &mut egui::Ui) {
        let (headline, accent) = self.headline_status();
        let detail_text = format!("状态: {}", self.status_message());

        let outer_rect = ui.available_rect_before_wrap();
        egui::Frame::new()
            .fill(egui::Color32::from_rgb(30, 34, 41))
            .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(60, 66, 74)))
            .inner_margin(egui::Margin {
                left: 18,
                right: 14,
                top: 12,
                bottom: 12,
            })
            .corner_radius(egui::CornerRadius::same(14))
            .show(ui, |ui| {
                ui.set_min_size(egui::vec2(0.0, 44.0));
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("●")
                                .font(FontId::proportional(12.0))
                                .color(accent),
                        );
                        ui.label(
                            RichText::new(match headline {
                                "已接管" => "正在加速",
                                "处理中" => "正在处理",
                                "异常" => "加速异常",
                                _ => "等待启动",
                            })
                            .font(FontId::proportional(16.0))
                            .strong()
                            .color(accent),
                        );
                    });
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(detail_text)
                            .font(FontId::proportional(12.0))
                            .color(egui::Color32::from_rgb(206, 212, 218)),
                    );
                });
            });

        // Draw accent color bar on the left edge
        let bar_rect = egui::Rect::from_min_size(
            outer_rect.min,
            egui::vec2(4.0, ui.min_rect().height().max(44.0)),
        );
        ui.painter().rect_filled(
            bar_rect,
            egui::CornerRadius {
                nw: 14,
                sw: 14,
                ne: 0,
                se: 0,
            },
            accent,
        );
    }

    fn render_action_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        panel_frame(
            egui::Color32::from_rgb(26, 30, 36),
            egui::Color32::from_rgb(50, 56, 64),
        )
        .show(ui, |ui| {
            let primary_label = if self.status.running {
                "停止加速"
            } else {
                "开始加速"
            };
            let (primary_fill, primary_text, primary_stroke) = if self.status.running {
                (
                    egui::Color32::from_rgb(186, 63, 21),
                    egui::Color32::from_rgb(248, 245, 243),
                    egui::Color32::from_rgb(223, 109, 51),
                )
            } else {
                (
                    egui::Color32::from_rgb(229, 171, 66),
                    egui::Color32::from_rgb(29, 24, 16),
                    egui::Color32::from_rgb(214, 158, 59),
                )
            };

            ui.spacing_mut().item_spacing = egui::vec2(10.0, 10.0);
            ui.vertical(|ui| {
                ui.columns(2, |columns| {
                    let left_width = columns[0].available_width();
                    if columns[0]
                        .add_sized(
                            [left_width, 68.0],
                            launcher_primary_button(
                                primary_label,
                                primary_fill,
                                primary_text,
                                primary_stroke,
                                egui::vec2(left_width, 68.0),
                                !self.busy,
                            ),
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

                    self.render_launcher_status_card(&mut columns[1]);
                });

                ui.horizontal(|ui| {
                    let button_width = ((ui.available_width() - 20.0) / 3.0).max(120.0);
                    if ui
                        .add(launcher_secondary_button(
                            "详情",
                            egui::vec2(button_width, 38.0),
                            true,
                        ))
                        .clicked()
                    {
                        self.navigate_to(ctx, UiPage::Details);
                    }
                    if ui
                        .add(launcher_secondary_button(
                            "设置",
                            egui::vec2(button_width, 38.0),
                            true,
                        ))
                        .clicked()
                    {
                        self.navigate_to(ctx, UiPage::Config);
                    }
                    if ui
                        .add(launcher_secondary_button(
                            "关于",
                            egui::vec2(button_width, 38.0),
                            true,
                        ))
                        .clicked()
                    {
                        self.navigate_to(ctx, UiPage::About);
                    }
                });
            });
        });
    }

    fn render_details_content(&self, ui: &mut egui::Ui) {
        self.render_brand_banner(ui, "状态详情", "运行状态、接管范围与故障提示");
        ui.add_space(8.0);
        if ui.available_width() >= 680.0 {
            ui.columns(2, |columns| {
                columns[0].spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                self.render_status_panel(&mut columns[0]);

                columns[1].spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                self.render_scope_panel(&mut columns[1]);
                self.render_tips_panel(&mut columns[1]);
            });
        } else {
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
            self.render_status_panel(ui);
            self.render_scope_panel(ui);
            self.render_tips_panel(ui);
        }
    }

    fn render_page_header(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, title: &str) {
        ui.horizontal(|ui| {
            if ui
                .add(subtle_button("← 返回", egui::vec2(80.0, 30.0), true))
                .clicked()
            {
                self.navigate_to(ctx, UiPage::Launcher);
            }
            ui.add_space(6.0);
            ui.vertical(|ui| {
                ui.label(
                    RichText::new(title)
                        .font(FontId::proportional(16.0))
                        .strong()
                        .color(egui::Color32::from_rgb(244, 245, 247)),
                );
                ui.label(
                    RichText::new("Linux.do Accelerator")
                        .font(FontId::proportional(10.5))
                        .color(egui::Color32::from_rgb(140, 150, 160)),
                );
            });
        });
        ui.add_space(8.0);
    }

    fn navigate_to(&mut self, ctx: &egui::Context, page: UiPage) {
        self.current_page = page;
        match page {
            UiPage::Launcher => {
                let size = egui::vec2(LAUNCHER_WINDOW_SIZE[0], LAUNCHER_WINDOW_SIZE[1]);
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
                ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(size));
                ctx.send_viewport_cmd(egui::ViewportCommand::MaxInnerSize(size));
                ctx.send_viewport_cmd(egui::ViewportCommand::Resizable(false));
            }
            UiPage::Details | UiPage::Config | UiPage::About => {
                let size = egui::vec2(DETAILS_WINDOW_SIZE[0], DETAILS_WINDOW_SIZE[1]);
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
                ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(egui::vec2(
                    720.0, 520.0,
                )));
                ctx.send_viewport_cmd(egui::ViewportCommand::MaxInnerSize(egui::vec2(
                    1200.0, 900.0,
                )));
                ctx.send_viewport_cmd(egui::ViewportCommand::Resizable(true));
            }
        }
        ctx.request_repaint();
    }

    fn render_status_panel(&self, ui: &mut egui::Ui) {
        panel_frame(
            egui::Color32::from_rgb(22, 26, 32),
            egui::Color32::from_rgb(50, 56, 64),
        )
        .show(ui, |ui| {
            ui.label(
                RichText::new("状态与日志")
                    .font(FontId::proportional(13.0))
                    .strong()
                    .color(egui::Color32::from_rgb(243, 179, 74)),
            );
            ui.add_space(2.0);
            ui.painter().line_segment(
                [
                    ui.cursor().left_top(),
                    ui.cursor().left_top() + egui::vec2(ui.available_width(), 0.0),
                ],
                egui::Stroke::new(1.0, egui::Color32::from_rgb(50, 56, 64)),
            );
            ui.add_space(6.0);
            egui::Frame::new()
                .fill(egui::Color32::from_rgb(17, 20, 25))
                .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(42, 48, 56)))
                .inner_margin(egui::Margin::symmetric(12, 10))
                .corner_radius(egui::CornerRadius::same(10))
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            RichText::new("当前状态")
                                .font(FontId::proportional(11.0))
                                .strong()
                                .color(egui::Color32::from_rgb(160, 170, 178)),
                        );
                        ui.label(
                            RichText::new(self.status.status_text.as_str())
                                .font(FontId::proportional(12.0))
                                .color(egui::Color32::from_rgb(232, 236, 239)),
                        );
                    });
                });
            ui.add_space(6.0);
            ui.label(
                RichText::new("最近错误")
                    .font(FontId::proportional(11.0))
                    .strong()
                    .color(egui::Color32::from_rgb(160, 170, 178)),
            );
            let details = self
                .status
                .last_error
                .as_deref()
                .unwrap_or("当前没有错误。运行异常时会直接显示真实原因。");
            ui.label(
                RichText::new(details)
                    .font(FontId::proportional(11.8))
                    .color(if self.status.last_error.is_some() {
                        egui::Color32::from_rgb(235, 110, 90)
                    } else {
                        egui::Color32::from_rgb(202, 208, 214)
                    }),
            );
            ui.add_space(7.0);
            ui.label(
                RichText::new("最近操作日志")
                    .font(FontId::proportional(11.0))
                    .strong()
                    .color(egui::Color32::from_rgb(160, 170, 178)),
            );
            egui::Frame::new()
                .fill(egui::Color32::from_rgb(14, 17, 21))
                .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(38, 44, 52)))
                .inner_margin(egui::Margin::symmetric(10, 8))
                .corner_radius(egui::CornerRadius::same(8))
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(228.0)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for line in self.recent_logs_or_placeholder() {
                                ui.label(
                                    RichText::new(line)
                                        .font(FontId::monospace(10.5))
                                        .color(egui::Color32::from_rgb(190, 198, 206)),
                                );
                            }
                        });
                });
        });
    }

    fn render_scope_panel(&self, ui: &mut egui::Ui) {
        panel_frame(
            egui::Color32::from_rgb(22, 26, 32),
            egui::Color32::from_rgb(50, 56, 64),
        )
        .show(ui, |ui| {
            ui.label(
                RichText::new("接管范围")
                    .font(FontId::proportional(13.0))
                    .strong()
                    .color(egui::Color32::from_rgb(243, 179, 74)),
            );
            ui.add_space(2.0);
            ui.painter().line_segment(
                [
                    ui.cursor().left_top(),
                    ui.cursor().left_top() + egui::vec2(ui.available_width(), 0.0),
                ],
                egui::Stroke::new(1.0, egui::Color32::from_rgb(50, 56, 64)),
            );
            ui.add_space(6.0);
            let hosts_count = self.config.hosts_domains().len().to_string();
            let doh_count = self.config.doh_endpoints.len().to_string();
            let cert_count = self.config.certificate_domains.len().to_string();
            ui.columns(3, |columns| {
                scope_metric_card(&mut columns[0], "域名", &hosts_count);
                scope_metric_card(&mut columns[1], "DoH", &doh_count);
                scope_metric_card(&mut columns[2], "证书", &cert_count);
            });
            ui.add_space(6.0);
            detail_value_row(ui, "上游", &self.config.upstream);
            detail_value_row(
                ui,
                "DoH 端点",
                &self
                    .config
                    .doh_endpoints
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "未配置".to_string()),
            );
        });
    }

    fn render_tips_panel(&self, ui: &mut egui::Ui) {
        panel_frame(
            egui::Color32::from_rgb(22, 26, 32),
            egui::Color32::from_rgb(50, 56, 64),
        )
        .show(ui, |ui| {
            ui.label(
                RichText::new("提示")
                    .font(FontId::proportional(13.0))
                    .strong()
                    .color(egui::Color32::from_rgb(243, 179, 74)),
            );
            ui.add_space(2.0);
            ui.painter().line_segment(
                [
                    ui.cursor().left_top(),
                    ui.cursor().left_top() + egui::vec2(ui.available_width(), 0.0),
                ],
                egui::Stroke::new(1.0, egui::Color32::from_rgb(50, 56, 64)),
            );
            ui.add_space(6.0);
            ui.label(
                RichText::new(
                    "DoH、接管域名和证书 SAN 都放在同一个 linuxdo-accelerator.toml 里，安装后直接改这一份即可。",
                )
                .font(FontId::proportional(11.4))
                .color(egui::Color32::from_rgb(214, 219, 223)),
            );
            ui.add_space(5.0);
            ui.label(
                RichText::new("重签根证书后，浏览器最好完全退出再重新打开一次。")
                    .font(FontId::proportional(10.9))
                    .color(egui::Color32::from_rgb(160, 171, 179)),
            );
            ui.add_space(3.0);
            ui.label(
                RichText::new("如果系统拒绝提权、DoH 不可用或端口监听失败，界面会直接显示真实原因。")
                    .font(FontId::proportional(10.9))
                    .color(egui::Color32::from_rgb(160, 171, 179)),
            );
        });
    }

    fn render_config_content(&self, ui: &mut egui::Ui) {
        ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
        self.render_brand_banner(
            ui,
            "设置总览",
            "统一读取单个配置文件，修改后重新启动即可生效",
        );
        ui.add_space(8.0);

        if ui.available_width() >= 680.0 {
            ui.columns(2, |columns| {
                columns[0].spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                panel_frame(
                    egui::Color32::from_rgb(22, 26, 32),
                    egui::Color32::from_rgb(50, 56, 64),
                )
                .show(&mut columns[0], |ui| {
                    ui.label(
                        RichText::new("配置文件")
                            .font(FontId::proportional(13.0))
                            .strong()
                            .color(egui::Color32::from_rgb(243, 179, 74)),
                    );
                    ui.add_space(6.0);
                    detail_value_row(ui, "主配置", &self.config_path.display().to_string());
                    detail_value_row(ui, "上游", &self.config.upstream);
                    detail_value_row(
                        ui,
                        "DoH 端点",
                        &self
                            .config
                            .doh_endpoints
                            .first()
                            .cloned()
                            .unwrap_or_else(|| "未配置".to_string()),
                    );
                });

                columns[1].spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                panel_frame(
                    egui::Color32::from_rgb(22, 26, 32),
                    egui::Color32::from_rgb(50, 56, 64),
                )
                .show(&mut columns[1], |ui| {
                    ui.label(
                        RichText::new("接管统计")
                            .font(FontId::proportional(13.0))
                            .strong()
                            .color(egui::Color32::from_rgb(243, 179, 74)),
                    );
                    ui.add_space(6.0);
                    ui.columns(3, |columns| {
                        scope_metric_card(
                            &mut columns[0],
                            "域名",
                            &self.config.proxy_domains.len().to_string(),
                        );
                        scope_metric_card(
                            &mut columns[1],
                            "DoH",
                            &self.config.doh_endpoints.len().to_string(),
                        );
                        scope_metric_card(
                            &mut columns[2],
                            "证书",
                            &self.config.certificate_domains.len().to_string(),
                        );
                    });
                    ui.add_space(8.0);
                    subtle_note(
                        ui,
                        "配置文件改完后重新点击开始加速即可，不需要手动清理页面状态。",
                    );
                });
            });
        } else {
            panel_frame(
                egui::Color32::from_rgb(22, 26, 32),
                egui::Color32::from_rgb(50, 56, 64),
            )
            .show(ui, |ui| {
                detail_value_row(ui, "主配置", &self.config_path.display().to_string());
                detail_value_row(ui, "上游", &self.config.upstream);
                detail_value_row(
                    ui,
                    "DoH 端点",
                    &self
                        .config
                        .doh_endpoints
                        .first()
                        .cloned()
                        .unwrap_or_else(|| "未配置".to_string()),
                );
            });
        }
    }

    fn render_about_content(&self, ui: &mut egui::Ui) {
        self.render_brand_banner(ui, "关于项目", "面向 linux.do 的轻量本地加速工具与桌面壳");
        ui.add_space(8.0);

        if ui.available_width() >= 680.0 {
            ui.columns(2, |columns| {
                columns[0].spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                panel_frame(
                    egui::Color32::from_rgb(22, 26, 32),
                    egui::Color32::from_rgb(50, 56, 64),
                )
                .show(&mut columns[0], |ui| {
                    ui.label(
                        RichText::new("产品信息")
                            .font(FontId::proportional(13.0))
                            .strong()
                            .color(egui::Color32::from_rgb(243, 179, 74)),
                    );
                    ui.add_space(6.0);
                    detail_value_row(ui, "名称", "Linux.do Accelerator");
                    detail_value_row(ui, "版本", &format!("v{APP_VERSION}"));
                    detail_value_row(ui, "形态", "原生 Rust 桌面壳 + CLI");
                });

                columns[1].spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                panel_frame(
                    egui::Color32::from_rgb(22, 26, 32),
                    egui::Color32::from_rgb(50, 56, 64),
                )
                .show(&mut columns[1], |ui| {
                    ui.label(
                        RichText::new("功能说明")
                            .font(FontId::proportional(13.0))
                            .strong()
                            .color(egui::Color32::from_rgb(243, 179, 74)),
                    );
                    ui.add_space(6.0);
                    about_bullet(
                        ui,
                        "支持证书安装、hosts 接管、本地 80/443 监听与 DoH 接管。",
                    );
                    about_bullet(ui, "DoH 与子域支持列表统一放在 linuxdo-accelerator.toml。");
                    about_bullet(ui, "点击加速会触发管理员提权，后台启动守护进程。");
                    about_bullet(
                        ui,
                        "如果提权失败、DoH 不可用或监听失败，界面会直接显示真实错误。",
                    );
                });
            });
        } else {
            panel_frame(
                egui::Color32::from_rgb(22, 26, 32),
                egui::Color32::from_rgb(50, 56, 64),
            )
            .show(ui, |ui| {
                detail_value_row(ui, "名称", "Linux.do Accelerator");
                detail_value_row(ui, "版本", &format!("v{APP_VERSION}"));
                detail_value_row(ui, "形态", "原生 Rust 桌面壳 + CLI");
                ui.add_space(6.0);
                about_bullet(
                    ui,
                    "支持证书安装、hosts 接管、本地 80/443 监听与 DoH 接管。",
                );
                about_bullet(ui, "DoH 与子域支持列表统一放在 linuxdo-accelerator.toml。");
            });
        }
    }

    fn show_confirm_action_dialog(&mut self, ctx: &egui::Context) {
        let Some(action) = self.confirm_action else {
            return;
        };

        let mut open = true;
        let mut confirmed = false;
        let mut cancelled = false;
        egui::Window::new(action.confirm_title())
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .default_width(500.0)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.set_width(500.0);
                ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);

                egui::ScrollArea::vertical()
                    .max_height(260.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new("该操作会再次申请管理员权限。")
                                .font(FontId::proportional(13.0))
                                .strong(),
                        );
                        ui.add_space(2.0);
                        ui.label(
                            RichText::new("• 开始加速：准备环境并启动本地代理。")
                                .font(FontId::proportional(11.5))
                                .color(egui::Color32::from_rgb(213, 218, 222)),
                        );
                        ui.label(
                            RichText::new(
                                "• 停止加速：自动停止服务、恢复 hosts，并尝试刷新 DNS 缓存。",
                            )
                            .font(FontId::proportional(11.5))
                            .color(egui::Color32::from_rgb(213, 218, 222)),
                        );

                        ui.add_space(2.0);
                        ui.label(
                            RichText::new("这些操作会再次申请管理员权限。")
                                .font(FontId::proportional(10.8))
                                .color(egui::Color32::from_rgb(165, 174, 182)),
                        );
                    });

                ui.add_space(10.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(subtle_button("取消", egui::vec2(92.0, 34.0), true))
                        .clicked()
                    {
                        cancelled = true;
                    }
                    if ui
                        .add(filled_button(
                            action.confirm_button(),
                            egui::Color32::from_rgb(243, 180, 66),
                            egui::Color32::from_rgb(24, 24, 22),
                            egui::Color32::from_rgb(216, 158, 58),
                            egui::vec2(172.0, 34.0),
                            !self.busy,
                        ))
                        .clicked()
                    {
                        confirmed = true;
                    }
                });
            });

        if confirmed {
            self.confirm_action = None;
            self.trigger_action(action);
        } else if cancelled || !open {
            self.confirm_action = None;
        }
    }

    #[cfg(target_os = "windows")]
    fn minimize_to_tray(&mut self, ctx: &egui::Context) {
        if let Some(tray) = &self.tray {
            let _ = tray.tray_icon.set_visible(true);
            self.hidden_to_tray = true;
            self.last_minimized = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            ctx.request_repaint();
        } else {
            self.feedback = "托盘不可用，已退回系统最小化".to_string();
            self.hidden_to_tray = false;
            self.last_minimized = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            ctx.request_repaint();
        }
    }

    #[cfg(target_os = "linux")]
    fn minimize_to_tray(&mut self, ctx: &egui::Context) {
        if let Some(tray) = &self.tray {
            let _ = tray.control_tx.send(TrayVisibilityCommand::Show);
            self.hidden_to_tray = true;
            self.last_minimized = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            ctx.request_repaint();
        } else {
            self.feedback = "托盘不可用，已退回系统最小化".to_string();
            self.hidden_to_tray = false;
            self.last_minimized = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            ctx.request_repaint();
        }
    }

    #[cfg(target_os = "macos")]
    fn minimize_to_tray(&mut self, ctx: &egui::Context) {
        match spawn_tray_shell(&self.config_path) {
            Ok(()) => {
                self.hidden_to_tray = true;
                self.last_minimized = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Err(error) => {
                self.hidden_to_tray = false;
                self.last_minimized = true;
                self.feedback = format!("托盘最小化失败，已退回系统最小化: {error}");
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                ctx.request_repaint();
            }
        }
    }

    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    fn minimize_to_tray(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
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

    #[cfg(target_os = "linux")]
    fn poll_tray_events(&mut self, ctx: &egui::Context) {
        while let Ok(command) = self.tray_rx.try_recv() {
            match command {
                TrayCommand::Restore => self.restore_from_tray(ctx),
                TrayCommand::Quit => {
                    #[cfg(target_os = "linux")]
                    if let Some(tray) = &self.tray {
                        let _ = tray.control_tx.send(TrayVisibilityCommand::Quit);
                    }
                    #[cfg(target_os = "linux")]
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    #[cfg(target_os = "windows")]
    fn restore_from_tray(&mut self, ctx: &egui::Context) {
        self.hidden_to_tray = false;
        self.last_minimized = true;
        if let Some(tray) = &self.tray {
            let _ = tray.tray_icon.set_visible(false);
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        ctx.request_repaint();
    }

    #[cfg(target_os = "windows")]
    fn poll_tray_events(&mut self, ctx: &egui::Context) {
        while let Ok(command) = self.tray_rx.try_recv() {
            match command {
                TrayCommand::Restore => self.restore_from_tray(ctx),
                TrayCommand::Quit => {
                    if let Some(tray) = &self.tray {
                        let _ = tray.tray_icon.set_visible(false);
                    }
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

    fn repaint_interval(&self) -> Duration {
        if self.busy || self.action_rx.is_some() || self.confirm_action.is_some() {
            ACTIVE_REPAINT_INTERVAL
        } else if self.hidden_to_tray {
            TRAY_REPAINT_INTERVAL
        } else {
            IDLE_REPAINT_INTERVAL
        }
    }
}

impl eframe::App for AcceleratorApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        #[cfg(target_os = "linux")]
        {
            self.poll_tray_events(ctx);
        }
        #[cfg(target_os = "windows")]
        {
            self.poll_tray_events(ctx);
        }
        #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
        self.sync_minimize_to_tray(ctx);

        self.poll_action();

        let repaint_interval = self.repaint_interval();
        if self.last_refresh.elapsed() >= repaint_interval {
            self.refresh_status();
            self.last_refresh = Instant::now();
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(egui::Color32::from_rgb(17, 20, 24)))
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(10.0, 10.0);
                self.render_window_title_bar(ui, ctx);
                ui.add_space(10.0);

                match self.current_page {
                    UiPage::Launcher => {
                        egui::Frame::new()
                            .fill(egui::Color32::from_rgb(20, 24, 29))
                            .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(44, 50, 58)))
                            .corner_radius(egui::CornerRadius::same(16))
                            .inner_margin(egui::Margin::same(14))
                            .show(ui, |ui| {
                                self.render_action_panel(ui, ctx);
                            });
                    }
                    UiPage::Details => {
                        panel_frame(
                            egui::Color32::from_rgb(20, 24, 29),
                            egui::Color32::from_rgb(44, 50, 58),
                        )
                        .show(ui, |ui| {
                            egui::ScrollArea::vertical()
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    self.render_page_header(ui, ctx, "状态详情");
                                    self.render_details_content(ui);
                                });
                        });
                    }
                    UiPage::Config => {
                        panel_frame(
                            egui::Color32::from_rgb(20, 24, 29),
                            egui::Color32::from_rgb(44, 50, 58),
                        )
                        .show(ui, |ui| {
                            egui::ScrollArea::vertical()
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    self.render_page_header(ui, ctx, "设置");
                                    self.render_config_content(ui);
                                });
                        });
                    }
                    UiPage::About => {
                        panel_frame(
                            egui::Color32::from_rgb(20, 24, 29),
                            egui::Color32::from_rgb(44, 50, 58),
                        )
                        .show(ui, |ui| {
                            egui::ScrollArea::vertical()
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    self.render_page_header(ui, ctx, "关于");
                                    self.render_about_content(ui);
                                });
                        });
                    }
                }
            });

        self.show_confirm_action_dialog(ctx);

        ctx.request_repaint_after(repaint_interval);
    }
}

#[derive(Clone, Copy)]
enum GuiAction {
    Start,
    Stop,
}

impl GuiAction {
    fn pending_message(self) -> &'static str {
        match self {
            Self::Start => "正在申请权限并启动加速...",
            Self::Stop => "正在停止加速并恢复 hosts...",
        }
    }

    fn subcommand(self) -> &'static str {
        match self {
            Self::Start => "helper-start",
            Self::Stop => "helper-stop",
        }
    }

    fn confirm_title(self) -> &'static str {
        "确认操作"
    }

    fn confirm_button(self) -> &'static str {
        "确认"
    }

    fn error_context(self) -> &'static str {
        match self {
            Self::Start => "elevation or command execution failed",
            Self::Stop => "failed to stop acceleration from GUI",
        }
    }
}

fn execute_action(config_path: &Path, action: GuiAction) -> Result<String> {
    #[cfg(target_os = "macos")]
    if matches!(action, GuiAction::Start) {
        service::prepare_certificate(Some(config_path.to_path_buf()))
            .with_context(|| "macOS certificate preparation failed")?;
    }

    let before_status = service::status(Some(config_path.to_path_buf())).unwrap_or_default();
    if let Ok(paths) = service::resolve_paths(Some(config_path.to_path_buf())) {
        let _ = append_runtime_log(
            &paths,
            "INFO",
            action.subcommand(),
            "GUI 已发起管理员操作请求",
        );
    }
    let cli_binary = locate_action_binary()?;
    let args = vec![
        "--config".to_string(),
        config_path.to_string_lossy().into_owned(),
        action.subcommand().to_string(),
    ];
    if let Err(error) = run_elevated(&cli_binary, &args) {
        if let Ok(status) = service::status(Some(config_path.to_path_buf())) {
            if let Some(last_error) = status.last_error.clone() {
                if let Ok(paths) = service::resolve_paths(Some(config_path.to_path_buf())) {
                    let _ = append_runtime_log(
                        &paths,
                        "ERROR",
                        action.subcommand(),
                        &format!("GUI 操作失败：{last_error}"),
                    );
                }
                return Err(Error::msg(last_error)).with_context(|| action.error_context());
            }
            if !matches!(action, GuiAction::Start) && service_state_changed(&before_status, &status)
            {
                if let Ok(paths) = service::resolve_paths(Some(config_path.to_path_buf())) {
                    let _ = append_runtime_log(
                        &paths,
                        "WARN",
                        action.subcommand(),
                        &format!("GUI 检测到状态已变化：{}", status.status_text),
                    );
                }
                return Err(Error::msg(status.status_text)).with_context(|| action.error_context());
            }
        }
        if let Ok(paths) = service::resolve_paths(Some(config_path.to_path_buf())) {
            let _ = append_runtime_log(
                &paths,
                "ERROR",
                action.subcommand(),
                &format!("GUI 提权执行失败：{error}"),
            );
        }
        return Err(error).with_context(|| action.error_context());
    }

    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        let status = service::status(Some(config_path.to_path_buf()))?;
        match action {
            GuiAction::Start if status.running => {
                return Ok("加速已启动，可以直接最小化窗口".to_string());
            }
            GuiAction::Stop if !status.running => {
                return Ok(status.status_text);
            }
            _ => {
                if let Some(error) = status.last_error.clone() {
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

fn service_state_changed(before: &ServiceState, after: &ServiceState) -> bool {
    before.running != after.running
        || before.pid != after.pid
        || before.status_text != after.status_text
        || before.last_error != after.last_error
        || before.updated_at != after.updated_at
}

fn load_recent_runtime_logs(config_path: &Path) -> Vec<String> {
    service::resolve_paths(Some(config_path.to_path_buf()))
        .ok()
        .and_then(|paths| read_recent_lines(&paths, 12).ok())
        .unwrap_or_default()
}

fn runtime_log_file_modified_at(config_path: &Path) -> Option<SystemTime> {
    service::resolve_paths(Some(config_path.to_path_buf()))
        .ok()
        .and_then(|paths| file_modified_at(&paths.runtime_log_path))
}

fn file_modified_at(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok()?.modified().ok()
}

#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
fn spawn_tray_shell(config_path: &Path) -> Result<()> {
    let gui_binary = locate_gui_binary()?;
    let args = vec![
        "--config".to_string(),
        config_path.to_string_lossy().into_owned(),
        "tray-shell".to_string(),
    ];
    #[cfg(target_os = "linux")]
    log_linux_tray_event(&format!(
        "spawn tray-shell exe={} config={}",
        gui_binary.display(),
        config_path.display()
    ));
    spawn_detached(&gui_binary, &args).context("failed to start tray shell")?;
    Ok(())
}

#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
fn spawn_ui_process(config_path: &Path) -> Result<()> {
    let gui_binary = locate_gui_binary()?;
    let args = vec![
        "--config".to_string(),
        config_path.to_string_lossy().into_owned(),
        "gui".to_string(),
    ];
    #[cfg(target_os = "linux")]
    log_linux_tray_event(&format!(
        "spawn ui exe={} config={}",
        gui_binary.display(),
        config_path.display()
    ));
    spawn_detached(&gui_binary, &args).context("failed to reopen UI")?;
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
    let (font_name, font_data) = load_ui_font();
    fonts.font_data.insert(font_name.clone(), font_data.into());
    if let Some(family) = fonts.families.get_mut(&FontFamily::Proportional) {
        family.push(font_name.clone());
    }
    if let Some(family) = fonts.families.get_mut(&FontFamily::Monospace) {
        family.push(font_name);
    }
    ctx.set_fonts(fonts);
}

fn load_ui_font() -> (String, egui::FontData) {
    if let Some((name, data)) = load_system_ui_font() {
        return (name, egui::FontData::from_owned(data));
    }

    (
        "linuxdo_cjk_embedded".to_string(),
        egui::FontData::from_static(EMBEDDED_CJK_FONT),
    )
}

fn load_system_ui_font() -> Option<(String, Vec<u8>)> {
    let mut database = fontdb::Database::new();
    database.load_system_fonts();

    for family_name in preferred_system_font_families() {
        let query = fontdb::Query {
            families: &[fontdb::Family::Name(family_name)],
            ..fontdb::Query::default()
        };
        let Some(id) = database.query(&query) else {
            continue;
        };
        let Some(info) = database.face(id) else {
            continue;
        };
        let font_name = info
            .families
            .first()
            .map(|(name, _)| name.clone())
            .unwrap_or_else(|| family_name.to_string());
        let Some(data) = database.with_face_data(id, |data, _| data.to_vec()) else {
            continue;
        };
        if data.len() <= EMBEDDED_CJK_FONT.len() {
            return Some((font_name, data));
        }
    }

    None
}

#[cfg(target_os = "windows")]
fn preferred_system_font_families() -> &'static [&'static str] {
    &["Microsoft YaHei UI", "Microsoft YaHei", "SimHei"]
}

#[cfg(target_os = "macos")]
fn preferred_system_font_families() -> &'static [&'static str] {
    &["PingFang SC", "Hiragino Sans GB"]
}

#[cfg(target_os = "linux")]
fn preferred_system_font_families() -> &'static [&'static str] {
    &[
        "Noto Sans CJK SC",
        "Noto Sans SC",
        "WenQuanYi Micro Hei",
        "Source Han Sans SC",
        "Droid Sans Fallback",
    ]
}

fn install_theme(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.override_text_color = Some(egui::Color32::from_rgb(232, 236, 239));
    style.visuals.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(24, 28, 34);
    style.visuals.widgets.noninteractive.fg_stroke.color = egui::Color32::from_rgb(232, 236, 239);
    style.visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(32, 37, 44);
    style.visuals.widgets.inactive.weak_bg_fill = egui::Color32::from_rgb(32, 37, 44);
    style.visuals.widgets.inactive.fg_stroke.color = egui::Color32::from_rgb(232, 236, 239);
    style.visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(44, 50, 58);
    style.visuals.widgets.hovered.weak_bg_fill = egui::Color32::from_rgb(44, 50, 58);
    style.visuals.widgets.hovered.fg_stroke.color = egui::Color32::from_rgb(252, 253, 254);
    style.visuals.widgets.active.bg_fill = egui::Color32::from_rgb(244, 184, 72);
    style.visuals.widgets.active.weak_bg_fill = egui::Color32::from_rgb(244, 184, 72);
    style.visuals.widgets.active.fg_stroke.color = egui::Color32::from_rgb(28, 24, 17);
    style.visuals.widgets.open.bg_fill = egui::Color32::from_rgb(35, 40, 47);
    style.visuals.widgets.open.fg_stroke.color = egui::Color32::from_rgb(232, 236, 239);
    style.visuals.selection.bg_fill = egui::Color32::from_rgb(244, 184, 72);
    style.visuals.selection.stroke.color = egui::Color32::from_rgb(28, 24, 17);
    style.visuals.window_fill = egui::Color32::from_rgb(17, 20, 24);
    style.visuals.panel_fill = egui::Color32::from_rgb(17, 20, 24);
    style.visuals.window_stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(54, 60, 67));
    style.visuals.extreme_bg_color = egui::Color32::from_rgb(12, 16, 20);
    style.visuals.faint_bg_color = egui::Color32::from_rgb(21, 25, 30);
    style.visuals.window_corner_radius = egui::CornerRadius::same(18);
    style.visuals.menu_corner_radius = egui::CornerRadius::same(10);
    style.visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(10);
    style.visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(10);
    style.visuals.widgets.active.corner_radius = egui::CornerRadius::same(10);
    style.visuals.window_shadow = egui::Shadow {
        offset: [0, 4],
        blur: 16,
        spread: 2,
        color: egui::Color32::from_rgba_unmultiplied(0, 0, 0, 80),
    };
    style.spacing.button_padding = egui::vec2(12.0, 7.0);
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.text_styles.insert(
        egui::TextStyle::Button,
        FontId::new(13.0, FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Body,
        FontId::new(13.0, FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Small,
        FontId::new(11.5, FontFamily::Proportional),
    );
    ctx.set_style(style);
}

fn panel_frame(fill: egui::Color32, stroke: egui::Color32) -> egui::Frame {
    egui::Frame::new()
        .fill(fill)
        .stroke(egui::Stroke::new(1.0, stroke))
        .inner_margin(egui::Margin::same(14))
        .corner_radius(egui::CornerRadius::same(14))
}

fn title_bar_button(
    label: &'static str,
    min_size: egui::Vec2,
    danger: bool,
    enabled: bool,
) -> egui::Button<'static> {
    let (fill, text, stroke) = if enabled {
        if danger {
            (
                egui::Color32::from_rgba_unmultiplied(200, 60, 50, 35),
                egui::Color32::from_rgb(248, 248, 250),
                egui::Color32::from_rgb(110, 55, 50),
            )
        } else {
            (
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 12),
                egui::Color32::from_rgb(220, 225, 230),
                egui::Color32::from_rgb(60, 66, 74),
            )
        }
    } else {
        (
            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 4),
            egui::Color32::from_rgb(112, 119, 127),
            egui::Color32::from_rgb(42, 48, 56),
        )
    };

    egui::Button::new(
        RichText::new(label)
            .font(FontId::proportional(14.0))
            .strong()
            .color(text),
    )
    .fill(fill)
    .stroke(egui::Stroke::new(1.0, stroke))
    .corner_radius(egui::CornerRadius::same(10))
    .min_size(min_size)
}

fn launcher_primary_button(
    label: &'static str,
    fill: egui::Color32,
    text: egui::Color32,
    stroke: egui::Color32,
    min_size: egui::Vec2,
    enabled: bool,
) -> egui::Button<'static> {
    let (fill, text, stroke) = if enabled {
        (fill, text, stroke)
    } else {
        (
            fill.linear_multiply(0.62),
            text.linear_multiply(0.88),
            stroke.linear_multiply(0.72),
        )
    };

    egui::Button::new(
        RichText::new(label)
            .font(FontId::proportional(17.0))
            .strong()
            .color(text),
    )
    .fill(fill)
    .stroke(egui::Stroke::new(1.5, stroke))
    .corner_radius(egui::CornerRadius::same(14))
    .min_size(min_size)
    .sense(if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    })
}

fn launcher_secondary_button(
    label: &'static str,
    min_size: egui::Vec2,
    enabled: bool,
) -> egui::Button<'static> {
    let (fill, text, stroke) = if enabled {
        (
            egui::Color32::from_rgb(34, 39, 46),
            egui::Color32::from_rgb(236, 240, 244),
            egui::Color32::from_rgb(70, 76, 84),
        )
    } else {
        (
            egui::Color32::from_rgb(28, 32, 38),
            egui::Color32::from_rgb(133, 140, 148),
            egui::Color32::from_rgb(56, 61, 69),
        )
    };

    egui::Button::new(
        RichText::new(label)
            .font(FontId::proportional(13.5))
            .strong()
            .color(text),
    )
    .fill(fill)
    .stroke(egui::Stroke::new(1.0, stroke))
    .corner_radius(egui::CornerRadius::same(10))
    .min_size(min_size)
}

fn filled_button(
    label: &'static str,
    fill: egui::Color32,
    text: egui::Color32,
    stroke: egui::Color32,
    min_size: egui::Vec2,
    enabled: bool,
) -> egui::Button<'static> {
    let (fill, text, stroke) = if enabled {
        (fill, text, stroke)
    } else {
        (
            fill.linear_multiply(0.62),
            text.linear_multiply(0.9),
            stroke.linear_multiply(0.68),
        )
    };
    egui::Button::new(
        RichText::new(label)
            .font(FontId::proportional(12.5))
            .strong()
            .color(text),
    )
    .fill(fill)
    .stroke(egui::Stroke::new(1.0, stroke))
    .corner_radius(egui::CornerRadius::same(10))
    .min_size(min_size)
    .sense(if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    })
}

fn subtle_button(
    label: &'static str,
    min_size: egui::Vec2,
    enabled: bool,
) -> egui::Button<'static> {
    let (fill, text, stroke) = if enabled {
        (
            egui::Color32::from_rgb(28, 33, 40),
            egui::Color32::from_rgb(236, 239, 242),
            egui::Color32::from_rgb(62, 68, 76),
        )
    } else {
        (
            egui::Color32::from_rgb(24, 28, 33),
            egui::Color32::from_rgb(126, 133, 141),
            egui::Color32::from_rgb(52, 58, 66),
        )
    };
    egui::Button::new(
        RichText::new(label)
            .font(FontId::proportional(11.8))
            .strong()
            .color(text),
    )
    .fill(fill)
    .stroke(egui::Stroke::new(1.0, stroke))
    .corner_radius(egui::CornerRadius::same(10))
    .min_size(min_size)
    .sense(if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    })
}

fn scope_metric_card(ui: &mut egui::Ui, label: &str, value: &str) {
    egui::Frame::new()
        .fill(egui::Color32::from_rgb(17, 20, 25))
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(42, 48, 56)))
        .inner_margin(egui::Margin::symmetric(10, 12))
        .corner_radius(egui::CornerRadius::same(10))
        .show(ui, |ui| {
            ui.set_min_size(egui::vec2(0.0, 64.0));
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new(label)
                        .font(FontId::proportional(10.5))
                        .strong()
                        .color(egui::Color32::from_rgb(150, 161, 170)),
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new(value)
                        .font(FontId::proportional(20.0))
                        .strong()
                        .color(egui::Color32::from_rgb(243, 179, 74)),
                );
            });
        });
}

fn subtle_note(ui: &mut egui::Ui, text: &str) {
    egui::Frame::new()
        .fill(egui::Color32::from_rgb(17, 20, 25))
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(42, 48, 56)))
        .inner_margin(egui::Margin::symmetric(12, 10))
        .corner_radius(egui::CornerRadius::same(10))
        .show(ui, |ui| {
            ui.label(
                RichText::new(text)
                    .font(FontId::proportional(11.2))
                    .color(egui::Color32::from_rgb(186, 194, 201)),
            );
        });
}

fn detail_value_row(ui: &mut egui::Ui, label: &str, value: &str) {
    egui::Frame::new()
        .fill(egui::Color32::from_rgb(17, 20, 25))
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(42, 48, 56)))
        .inner_margin(egui::Margin::symmetric(12, 10))
        .corner_radius(egui::CornerRadius::same(10))
        .show(ui, |ui| {
            ui.label(
                RichText::new(label)
                    .font(FontId::proportional(11.0))
                    .strong()
                    .color(egui::Color32::from_rgb(243, 179, 74)),
            );
            ui.add_space(3.0);
            ui.label(
                RichText::new(value)
                    .font(FontId::monospace(11.2))
                    .color(egui::Color32::from_rgb(232, 236, 239)),
            );
        });
}

fn about_bullet(ui: &mut egui::Ui, text: &str) {
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new("•")
                .font(FontId::proportional(14.0))
                .strong()
                .color(egui::Color32::from_rgb(243, 179, 74)),
        );
        ui.label(
            RichText::new(text)
                .font(FontId::proportional(12.0))
                .color(egui::Color32::from_rgb(214, 219, 223)),
        );
    });
}

#[cfg(target_os = "windows")]
fn build_windows_tray_state(
    ctx: &egui::Context,
) -> (Option<TrayState>, Receiver<TrayCommand>) {
    let (event_tx, event_rx) = mpsc::channel();

    let menu = Menu::new();
    let show_item = MenuItem::with_id("tray-show", "打开窗口", true, None);
    let quit_item = MenuItem::with_id("tray-quit", "退出程序", true, None);
    if menu
        .append_items(&[&show_item, &PredefinedMenuItem::separator(), &quit_item])
        .is_err()
    {
        return (None, event_rx);
    }

    let tray_icon = match tray_window_icon() {
        Ok(icon) => TrayIconBuilder::new()
            .with_id("linuxdo-accelerator-tray")
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip("Linux.do Accelerator")
            .with_icon(icon)
            .build()
            .ok(),
        Err(_) => None,
    };

    let Some(tray_icon) = tray_icon else {
        return (None, event_rx);
    };
    let _ = tray_icon.set_visible(false);

    let show_id = show_item.id().clone();
    let quit_id = quit_item.id().clone();
    let event_tx_click = event_tx.clone();
    let ctx_menu = ctx.clone();
    let ctx_tray = ctx.clone();

    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if event.id == show_id {
            let _ = event_tx.send(TrayCommand::Restore);
            ctx_menu.request_repaint();
        } else if event.id == quit_id {
            let _ = event_tx.send(TrayCommand::Quit);
            ctx_menu.request_repaint();
        }
    }));

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
            let _ = event_tx_click.send(TrayCommand::Restore);
            ctx_tray.request_repaint();
        }
        _ => {}
    }));

    (Some(TrayState { tray_icon }), event_rx)
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
#[allow(dead_code)]
fn schedule_windows_shortcut_icon_refresh(_config_path: &Path) {}
