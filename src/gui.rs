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
use crate::hosts::validate_hosts_backup_file;
use crate::hosts_store::{BackupState, backup_state};
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
use crate::runtime_log::{append as append_runtime_log, read_recent_lines};
use crate::service;
use crate::state::ServiceState;

const APP_WINDOW_TITLE: &str = "Linux.do Accelerator";
const APP_ID: &str = "linuxdo-accelerator";
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const ACTIVE_REPAINT_INTERVAL: Duration = Duration::from_millis(100);
const IDLE_REPAINT_INTERVAL: Duration = Duration::from_secs(5);
const TRAY_REPAINT_INTERVAL: Duration = Duration::from_secs(15);

pub fn run(config_path: PathBuf) -> Result<()> {
    let native_options = eframe::NativeOptions {
        renderer: default_renderer(),
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_WINDOW_TITLE)
            .with_app_id(APP_ID)
            .with_icon(branding::icon_data(256))
            .with_inner_size([1120.0, 820.0])
            .with_min_inner_size([920.0, 760.0])
            .with_max_inner_size([1440.0, 1080.0])
            .with_minimize_button(!cfg!(target_os = "linux"))
            .with_maximize_button(true)
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
    hosts_backup_state: BackupState,
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
        let config_modified_at = file_modified_at(&config_path);
        let status = service::status(Some(config_path.clone())).unwrap_or_default();
        let hosts_backup_state = load_hosts_backup_state(&config_path);
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
            hosts_backup_state,
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
        self.hosts_backup_state = load_hosts_backup_state(&self.config_path);
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
                                Some(GuiAction::RestoreHosts) | Some(GuiAction::Cleanup) => {
                                    self.optimistic_running = None;
                                    self.refresh_status();
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

    fn hosts_backup_badge(&self) -> (&'static str, egui::Color32) {
        match self.hosts_backup_state {
            BackupState::Ready => ("已检测到备份", egui::Color32::from_rgb(106, 220, 155)),
            BackupState::Missing => ("未检测到备份", egui::Color32::from_rgb(250, 196, 92)),
            BackupState::Inconsistent => ("备份状态异常", egui::Color32::from_rgb(255, 120, 100)),
        }
    }

    fn maintenance_hint(&self) -> &'static str {
        if self.status.running {
            return "恢复 hosts 会先自动停止加速再恢复；彻底恢复会自动停止并清理。";
        }

        match self.hosts_backup_state {
            BackupState::Ready => "恢复 hosts 只恢复 hosts；彻底恢复还会卸载证书并清理状态。",
            BackupState::Missing => {
                "未发现 hosts 备份；仍可用“彻底恢复原始状态”清理程序写入的规则。"
            }
            BackupState::Inconsistent => {
                "hosts 备份状态异常；建议直接用“彻底恢复原始状态”尽量回到初始状态。"
            }
        }
    }

    fn can_restore_hosts(&self) -> bool {
        !self.busy && self.hosts_backup_state == BackupState::Ready
    }

    fn confirm_summary(&self, action: GuiAction) -> String {
        match action {
            GuiAction::RestoreHosts => {
                "会用首次接管前的备份覆盖当前 hosts 文件；如果加速正在运行，会先自动停止。"
                    .to_string()
            }
            GuiAction::Cleanup => {
                "会停止加速，并尽量把本程序对系统做的改动恢复到初始状态。".to_string()
            }
            GuiAction::Start | GuiAction::Stop => "确认执行该操作。".to_string(),
        }
    }

    fn confirm_details(&self, action: GuiAction) -> Vec<String> {
        match action {
            GuiAction::RestoreHosts => vec![
                "仅恢复 hosts 文件，不会卸载根证书。".to_string(),
                "如果当前加速正在运行，会先自动停止加速服务。".to_string(),
                "恢复后会尝试刷新系统 DNS 缓存。".to_string(),
                "恢复完成后，如需再次加速，可重新点击“开始加速”。".to_string(),
            ],
            GuiAction::Cleanup => {
                let mut lines = vec![
                    "会停止当前加速服务。".to_string(),
                    "会移除回环别名并清理运行状态。".to_string(),
                    "会卸载根证书；后续如需继续使用，需要重新安装。".to_string(),
                ];
                match self.hosts_backup_state {
                    BackupState::Ready => lines.insert(
                        1,
                        "当前已检测到完整 hosts 备份，会优先恢复首次接管前的原始内容。"
                            .to_string(),
                    ),
                    BackupState::Missing => lines.insert(
                        1,
                        "当前未检测到完整 hosts 备份，无法保证完整恢复原始 hosts；会退化为仅清理本程序写入的规则。"
                            .to_string(),
                    ),
                    BackupState::Inconsistent => lines.insert(
                        1,
                        "当前 hosts 备份状态异常，无法直接做完整恢复；会退化为仅清理本程序写入的规则。"
                            .to_string(),
                    ),
                }
                lines
            }
            GuiAction::Start | GuiAction::Stop => Vec::new(),
        }
    }

    fn confirm_status_note(&self, action: GuiAction) -> Option<(String, egui::Color32)> {
        match action {
            GuiAction::Cleanup => {
                let (label, color) = self.hosts_backup_badge();
                Some((format!("当前检测结果：{label}"), color))
            }
            GuiAction::RestoreHosts => Some((
                if self.status.running {
                    "当前状态：加速中，确认后会先自动停止，再恢复 hosts。".to_string()
                } else {
                    "当前状态：已停止，可直接恢复 hosts。".to_string()
                },
                if self.status.running {
                    egui::Color32::from_rgb(250, 196, 92)
                } else {
                    egui::Color32::from_rgb(106, 220, 155)
                },
            )),
            GuiAction::Start | GuiAction::Stop => None,
        }
    }

    fn render_header(&self, ui: &mut egui::Ui) {
        let (headline, accent) = self.headline_status();
        ui.horizontal(|ui| {
            ui.add(egui::Image::new((self.logo.id(), egui::vec2(46.0, 46.0))));
            ui.vertical(|ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new("Linux.do Accelerator")
                            .font(FontId::proportional(22.0))
                            .strong()
                            .color(egui::Color32::from_rgb(248, 246, 239)),
                    );
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(format!("v{APP_VERSION}"))
                            .font(FontId::proportional(12.5))
                            .color(egui::Color32::from_rgb(145, 161, 175)),
                    );
                    ui.add_space(8.0);
                    egui::Frame::new()
                        .fill(accent.linear_multiply(0.16))
                        .stroke(egui::Stroke::new(1.0, accent.linear_multiply(0.72)))
                        .inner_margin(egui::Margin::symmetric(10, 5))
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
                ui.label(
                    RichText::new(
                        "本地加速、证书接管、hosts 管理与运行状态统一收口到一个桌面入口。",
                    )
                    .font(FontId::proportional(12.5))
                    .color(egui::Color32::from_rgb(163, 175, 185)),
                );
            });
        });
    }

    fn render_action_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        panel_frame(
            egui::Color32::from_rgb(24, 31, 38),
            egui::Color32::from_rgb(45, 58, 69),
        )
        .show(ui, |ui| {
            ui.label(
                RichText::new("操作")
                    .font(FontId::proportional(12.5))
                    .strong()
                    .color(egui::Color32::from_rgb(248, 194, 96)),
            );
            ui.add_space(2.0);
            ui.label(
                RichText::new(self.status_message())
                    .font(FontId::proportional(15.0))
                    .color(egui::Color32::from_rgb(237, 239, 241)),
            );
            ui.add_space(8.0);

            let primary_label = if self.status.running {
                "停止加速"
            } else {
                "开始加速"
            };
            let (primary_fill, primary_text, primary_stroke) = if self.status.running {
                (
                    egui::Color32::from_rgb(180, 76, 64),
                    egui::Color32::from_rgb(252, 247, 245),
                    egui::Color32::from_rgb(209, 108, 93),
                )
            } else {
                (
                    egui::Color32::from_rgb(244, 184, 72),
                    egui::Color32::from_rgb(30, 26, 18),
                    egui::Color32::from_rgb(219, 162, 58),
                )
            };

            if ui
                .add(filled_button(
                    primary_label,
                    primary_fill,
                    primary_text,
                    primary_stroke,
                    egui::vec2(ui.available_width(), 46.0),
                    !self.busy,
                ))
                .clicked()
            {
                let action = if self.status.running {
                    GuiAction::Stop
                } else {
                    GuiAction::Start
                };
                self.trigger_action(action);
            }

            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add(subtle_button("最小化", egui::vec2(96.0, 34.0), !self.busy))
                    .clicked()
                {
                    self.minimize_to_tray(ctx);
                }
                if ui
                    .add(subtle_button("设置", egui::vec2(78.0, 34.0), true))
                    .clicked()
                {
                    self.show_config = true;
                }
                if ui
                    .add(subtle_button("关于", egui::vec2(78.0, 34.0), true))
                    .clicked()
                {
                    self.show_about = true;
                }
            });
        });
    }

    fn render_maintenance_panel(&mut self, ui: &mut egui::Ui) {
        panel_frame(
            egui::Color32::from_rgb(38, 29, 22),
            egui::Color32::from_rgb(86, 63, 49),
        )
        .show(ui, |ui| {
            let (backup_label, backup_color) = self.hosts_backup_badge();
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new("恢复与清理")
                        .font(FontId::proportional(12.5))
                        .strong()
                        .color(egui::Color32::from_rgb(255, 200, 128)),
                );
                egui::Frame::new()
                    .fill(backup_color.linear_multiply(0.14))
                    .stroke(egui::Stroke::new(1.0, backup_color.linear_multiply(0.7)))
                    .inner_margin(egui::Margin::symmetric(8, 4))
                    .corner_radius(egui::CornerRadius::same(255))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(backup_label)
                                .font(FontId::proportional(10.5))
                                .strong()
                                .color(backup_color),
                        );
                    });
            });
            ui.add_space(4.0);
            ui.label(
                RichText::new(self.maintenance_hint())
                    .font(FontId::proportional(11.8))
                    .color(egui::Color32::from_rgb(229, 231, 233)),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new("这些操作会再次弹出管理员确认。")
                    .font(FontId::proportional(10.8))
                    .color(egui::Color32::from_rgb(177, 184, 190)),
            );
            ui.add_space(10.0);

            if ui
                .add(filled_button(
                    "恢复 hosts",
                    egui::Color32::from_rgb(46, 102, 86),
                    egui::Color32::from_rgb(245, 249, 247),
                    egui::Color32::from_rgb(68, 136, 116),
                    egui::vec2(ui.available_width(), 38.0),
                    self.can_restore_hosts(),
                ))
                .clicked()
            {
                self.confirm_action = Some(GuiAction::RestoreHosts);
            }

            if ui
                .add(filled_button(
                    "彻底恢复原始状态",
                    egui::Color32::from_rgb(136, 63, 56),
                    egui::Color32::from_rgb(252, 247, 245),
                    egui::Color32::from_rgb(176, 86, 77),
                    egui::vec2(ui.available_width(), 38.0),
                    !self.busy,
                ))
                .clicked()
            {
                self.confirm_action = Some(GuiAction::Cleanup);
            }
        });
    }

    fn render_status_panel(&self, ui: &mut egui::Ui) {
        panel_frame(
            egui::Color32::from_rgb(21, 26, 32),
            egui::Color32::from_rgb(40, 49, 58),
        )
        .show(ui, |ui| {
            ui.label(
                RichText::new("状态与日志")
                    .font(FontId::proportional(12.0))
                    .strong()
                    .color(egui::Color32::from_rgb(159, 173, 183)),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!("当前状态：{}", self.status.status_text))
                    .font(FontId::proportional(12.0))
                    .color(egui::Color32::from_rgb(229, 233, 236)),
            );
            ui.add_space(6.0);
            ui.label(
                RichText::new("最近错误")
                    .font(FontId::proportional(10.8))
                    .strong()
                    .color(egui::Color32::from_rgb(171, 180, 187)),
            );
            let details = self
                .status
                .last_error
                .as_deref()
                .unwrap_or("当前没有错误。运行异常时会直接显示真实原因。");
            ui.label(
                RichText::new(details)
                    .font(FontId::proportional(11.4))
                    .color(if self.status.last_error.is_some() {
                        egui::Color32::from_rgb(255, 129, 108)
                    } else {
                        egui::Color32::from_rgb(202, 208, 214)
                    }),
            );
            ui.add_space(8.0);
            ui.label(
                RichText::new("最近操作日志")
                    .font(FontId::proportional(10.8))
                    .strong()
                    .color(egui::Color32::from_rgb(171, 180, 187)),
            );
            egui::ScrollArea::vertical()
                .max_height(188.0)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for line in self.recent_logs_or_placeholder() {
                        ui.label(
                            RichText::new(line)
                                .font(FontId::monospace(10.5))
                                .color(egui::Color32::from_rgb(204, 210, 216)),
                        );
                    }
                });
        });
    }

    fn render_scope_panel(&self, ui: &mut egui::Ui) {
        panel_frame(
            egui::Color32::from_rgb(27, 25, 19),
            egui::Color32::from_rgb(73, 65, 43),
        )
        .show(ui, |ui| {
            ui.label(
                RichText::new("接管范围")
                    .font(FontId::proportional(12.5))
                    .strong()
                    .color(egui::Color32::from_rgb(247, 193, 90)),
            );
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                compact_metric(ui, "域名", &self.config.proxy_domains.len().to_string());
                compact_metric(ui, "DoH", &self.config.doh_endpoints.len().to_string());
                compact_metric(
                    ui,
                    "证书",
                    &self.config.certificate_domains.len().to_string(),
                );
            });
            ui.add_space(8.0);
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
    }

    fn render_tips_panel(&self, ui: &mut egui::Ui) {
        panel_frame(
            egui::Color32::from_rgb(21, 29, 31),
            egui::Color32::from_rgb(44, 60, 59),
        )
        .show(ui, |ui| {
            ui.label(
                RichText::new("提示")
                    .font(FontId::proportional(12.5))
                    .strong()
                    .color(egui::Color32::from_rgb(125, 209, 176)),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "DoH、接管域名和证书 SAN 都放在同一个 linuxdo-accelerator.toml 里，安装后直接改这一份即可。",
                )
                .font(FontId::proportional(11.8))
                .color(egui::Color32::from_rgb(214, 219, 223)),
            );
            ui.add_space(6.0);
            ui.label(
                RichText::new("重签根证书后，浏览器最好完全退出再重新打开一次。")
                    .font(FontId::proportional(11.2))
                    .color(egui::Color32::from_rgb(160, 171, 179)),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new("如果系统拒绝提权、DoH 不可用或端口监听失败，界面会直接显示真实原因。")
                    .font(FontId::proportional(11.2))
                    .color(egui::Color32::from_rgb(160, 171, 179)),
            );
        });
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
                            RichText::new(self.confirm_summary(action))
                                .font(FontId::proportional(13.0))
                                .strong(),
                        );

                        if let Some((note, color)) = self.confirm_status_note(action) {
                            ui.add_space(2.0);
                            egui::Frame::new()
                                .fill(color.linear_multiply(0.12))
                                .stroke(egui::Stroke::new(1.0, color.linear_multiply(0.72)))
                                .inner_margin(egui::Margin::symmetric(10, 8))
                                .corner_radius(egui::CornerRadius::same(10))
                                .show(ui, |ui| {
                                    ui.label(
                                        RichText::new(note)
                                            .font(FontId::proportional(11.2))
                                            .strong()
                                            .color(color),
                                    );
                                });
                        }

                        ui.add_space(2.0);
                        for line in self.confirm_details(action) {
                            ui.label(
                                RichText::new(format!("• {line}"))
                                    .font(FontId::proportional(11.5))
                                    .color(egui::Color32::from_rgb(213, 218, 222)),
                            );
                        }

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
        #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
        {
            self.poll_tray_events(ctx);
            self.sync_minimize_to_tray(ctx);
        }

        self.poll_action();

        let repaint_interval = self.repaint_interval();
        if self.last_refresh.elapsed() >= repaint_interval {
            self.refresh_status();
            self.last_refresh = Instant::now();
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(10.0, 10.0);
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    egui::Frame::new()
                        .fill(egui::Color32::from_rgb(15, 19, 24))
                        .inner_margin(egui::Margin::same(18))
                        .corner_radius(egui::CornerRadius::same(24))
                        .show(ui, |ui| {
                            self.render_header(ui);
                            ui.add_space(16.0);

                            if ui.available_width() >= 900.0 {
                                ui.columns(2, |columns| {
                                    columns[0].spacing_mut().item_spacing = egui::vec2(10.0, 10.0);
                                    self.render_action_panel(&mut columns[0], ctx);
                                    self.render_maintenance_panel(&mut columns[0]);
                                    self.render_status_panel(&mut columns[0]);

                                    columns[1].spacing_mut().item_spacing = egui::vec2(10.0, 10.0);
                                    self.render_scope_panel(&mut columns[1]);
                                    self.render_tips_panel(&mut columns[1]);
                                });
                            } else {
                                ui.spacing_mut().item_spacing = egui::vec2(10.0, 10.0);
                                self.render_action_panel(ui, ctx);
                                self.render_scope_panel(ui);
                                self.render_maintenance_panel(ui);
                                self.render_status_panel(ui);
                                self.render_tips_panel(ui);
                            }
                        });
                });
        });

        self.show_confirm_action_dialog(ctx);

        if self.show_config {
            egui::Window::new("设置")
                .collapsible(false)
                .resizable(true)
                .default_width(700.0)
                .default_height(360.0)
                .show(ctx, |ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(10.0, 10.0);
                    ui.label(
                        RichText::new("配置总览")
                            .font(FontId::proportional(16.0))
                            .strong(),
                    );
                    ui.label("当前使用单配置文件，修改后重新启动加速即可生效。");
                    ui.separator();
                    compact_row(ui, "主配置", &self.config_path.display().to_string());
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
                    ui.add_space(6.0);
                    ui.horizontal_wrapped(|ui| {
                        compact_metric(
                            ui,
                            "接管域名",
                            &self.config.proxy_domains.len().to_string(),
                        );
                        compact_metric(
                            ui,
                            "DoH 数量",
                            &self.config.doh_endpoints.len().to_string(),
                        );
                        compact_metric(
                            ui,
                            "证书 SAN",
                            &self.config.certificate_domains.len().to_string(),
                        );
                    });
                    ui.add_space(10.0);
                    if ui
                        .add(subtle_button("关闭", egui::vec2(92.0, 34.0), true))
                        .clicked()
                    {
                        self.show_config = false;
                    }
                });
        }

        if self.show_about {
            egui::Window::new("关于")
                .collapsible(false)
                .resizable(true)
                .default_width(620.0)
                .default_height(360.0)
                .show(ctx, |ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        ui.add(egui::Image::new((self.logo.id(), egui::vec2(46.0, 46.0))));
                        ui.label(
                            RichText::new("Linux.do Accelerator")
                                .font(FontId::proportional(18.0))
                                .strong(),
                        );
                        ui.label(format!("版本 v{APP_VERSION}"));
                        ui.label("原生 Rust 桌面壳 + CLI");
                        ui.label(
                            "支持证书安装、hosts 接管、本地 80/443 监听和 Android VPN DNS 接管。",
                        );
                        ui.label("DoH 与子域支持列表统一放在 linuxdo-accelerator.toml。");
                        ui.label("点击加速会触发管理员提权，后台启动守护进程。");
                        ui.label(
                            "如果系统拒绝提权、DoH 不可用或端口监听失败，界面会直接显示真实错误。",
                        );
                    });
                    if ui
                        .add(subtle_button("关闭", egui::vec2(92.0, 34.0), true))
                        .clicked()
                    {
                        self.show_about = false;
                    }
                });
        }

        ctx.request_repaint_after(repaint_interval);
    }
}

#[derive(Clone, Copy)]
enum GuiAction {
    Start,
    Stop,
    RestoreHosts,
    Cleanup,
}

impl GuiAction {
    fn pending_message(self) -> &'static str {
        match self {
            Self::Start => "正在申请权限并启动加速...",
            Self::Stop => "正在停止加速...",
            Self::RestoreHosts => "正在恢复 hosts 备份...",
            Self::Cleanup => "正在恢复原始状态...",
        }
    }

    fn subcommand(self) -> &'static str {
        match self {
            Self::Start => "helper-start",
            Self::Stop => "helper-stop",
            Self::RestoreHosts => "restore-hosts",
            Self::Cleanup => "cleanup",
        }
    }

    fn confirm_title(self) -> &'static str {
        match self {
            Self::RestoreHosts => "确认恢复 hosts",
            Self::Cleanup => "确认彻底恢复原始状态",
            Self::Start | Self::Stop => "确认操作",
        }
    }

    fn confirm_button(self) -> &'static str {
        match self {
            Self::RestoreHosts => "确认恢复 hosts",
            Self::Cleanup => "确认恢复原始状态",
            Self::Start | Self::Stop => "确认",
        }
    }

    fn fallback_success_message(self) -> &'static str {
        match self {
            Self::Start => "加速已启动，可以直接最小化窗口",
            Self::Stop => "加速已停止",
            Self::RestoreHosts => "hosts 已恢复为备份",
            Self::Cleanup => "已恢复原始状态",
        }
    }

    fn error_context(self) -> &'static str {
        match self {
            Self::Start => "elevation or command execution failed",
            Self::Stop => "failed to stop acceleration from GUI",
            Self::RestoreHosts => "failed to restore hosts from GUI",
            Self::Cleanup => "failed to cleanup accelerator state from GUI",
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
                return Ok("加速已停止".to_string());
            }
            GuiAction::RestoreHosts | GuiAction::Cleanup => {
                if let Some(error) = status.last_error.clone() {
                    bail!(error);
                }
                if service_state_changed(&before_status, &status) {
                    return Ok(status.status_text);
                }
                return Ok(action.fallback_success_message().to_string());
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

fn load_hosts_backup_state(config_path: &Path) -> BackupState {
    service::resolve_paths(Some(config_path.to_path_buf()))
        .map(|paths| match backup_state(&paths) {
            BackupState::Ready => {
                if validate_hosts_backup_file(&paths).is_ok() {
                    BackupState::Ready
                } else {
                    BackupState::Inconsistent
                }
            }
            state => state,
        })
        .unwrap_or(BackupState::Missing)
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

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
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
    style.visuals.override_text_color = Some(egui::Color32::from_rgb(232, 236, 239));
    style.visuals.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(21, 25, 30);
    style.visuals.widgets.noninteractive.fg_stroke.color = egui::Color32::from_rgb(232, 236, 239);
    style.visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(34, 42, 50);
    style.visuals.widgets.inactive.weak_bg_fill = egui::Color32::from_rgb(34, 42, 50);
    style.visuals.widgets.inactive.fg_stroke.color = egui::Color32::from_rgb(232, 236, 239);
    style.visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(47, 58, 69);
    style.visuals.widgets.hovered.weak_bg_fill = egui::Color32::from_rgb(47, 58, 69);
    style.visuals.widgets.hovered.fg_stroke.color = egui::Color32::from_rgb(248, 249, 250);
    style.visuals.widgets.active.bg_fill = egui::Color32::from_rgb(244, 184, 72);
    style.visuals.widgets.active.weak_bg_fill = egui::Color32::from_rgb(244, 184, 72);
    style.visuals.widgets.active.fg_stroke.color = egui::Color32::from_rgb(28, 24, 17);
    style.visuals.widgets.open.bg_fill = egui::Color32::from_rgb(40, 49, 57);
    style.visuals.widgets.open.fg_stroke.color = egui::Color32::from_rgb(232, 236, 239);
    style.visuals.selection.bg_fill = egui::Color32::from_rgb(244, 184, 72);
    style.visuals.selection.stroke.color = egui::Color32::from_rgb(28, 24, 17);
    style.visuals.window_fill = egui::Color32::from_rgb(12, 16, 20);
    style.visuals.panel_fill = egui::Color32::from_rgb(10, 14, 18);
    style.visuals.window_stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(49, 57, 66));
    style.visuals.extreme_bg_color = egui::Color32::from_rgb(12, 16, 20);
    style.visuals.faint_bg_color = egui::Color32::from_rgb(19, 24, 28);
    style.visuals.window_corner_radius = egui::CornerRadius::same(20);
    style.visuals.menu_corner_radius = egui::CornerRadius::same(12);
    style.visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(12);
    style.visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(12);
    style.visuals.widgets.active.corner_radius = egui::CornerRadius::same(12);
    style.spacing.button_padding = egui::vec2(14.0, 9.0);
    style.spacing.item_spacing = egui::vec2(10.0, 10.0);
    style.text_styles.insert(
        egui::TextStyle::Button,
        FontId::new(12.5, FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Body,
        FontId::new(12.5, FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Small,
        FontId::new(11.0, FontFamily::Proportional),
    );
    ctx.set_style(style);
}

fn panel_frame(fill: egui::Color32, stroke: egui::Color32) -> egui::Frame {
    egui::Frame::new()
        .fill(fill)
        .stroke(egui::Stroke::new(1.0, stroke))
        .inner_margin(egui::Margin::same(14))
        .corner_radius(egui::CornerRadius::same(16))
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
            .font(FontId::proportional(13.0))
            .strong()
            .color(text),
    )
    .fill(fill)
    .stroke(egui::Stroke::new(1.0, stroke))
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
            egui::Color32::from_rgb(38, 46, 54),
            egui::Color32::from_rgb(234, 238, 241),
            egui::Color32::from_rgb(60, 72, 82),
        )
    } else {
        (
            egui::Color32::from_rgb(56, 66, 76),
            egui::Color32::from_rgb(224, 229, 234),
            egui::Color32::from_rgb(86, 98, 109),
        )
    };
    egui::Button::new(
        RichText::new(label)
            .font(FontId::proportional(12.2))
            .strong()
            .color(text),
    )
    .fill(fill)
    .stroke(egui::Stroke::new(1.0, stroke))
    .min_size(min_size)
    .sense(if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    })
}

fn compact_metric(ui: &mut egui::Ui, label: &str, value: &str) {
    egui::Frame::new()
        .fill(egui::Color32::from_rgb(30, 32, 28))
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(69, 67, 49)))
        .inner_margin(egui::Margin::symmetric(10, 8))
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
