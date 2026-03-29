use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
#[cfg(target_family = "unix")]
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use tokio::sync::watch;

use crate::certs::{ensure_bundle, load_or_create_bundle};
use crate::config::AppConfig;
use crate::hosts::{
    apply_hosts, backup_hosts_file, hosts_are_applied, remove_hosts, restore_hosts_file,
    validate_hosts_backup_file,
};
use crate::hosts_store::{BackupState, backup_state, clear_hosts_backup};
use crate::paths::AppPaths;
use crate::platform::{
    ensure_elevated, ensure_loopback_alias, flush_dns_cache, install_ca, is_process_running,
    remove_loopback_alias, spawn_detached, terminate_process, terminate_process_force,
    uninstall_ca,
};
use crate::proxy::run_proxy;
use crate::runtime_log;
use crate::state;

pub fn resolve_paths(config_override: Option<PathBuf>) -> Result<AppPaths> {
    let paths = AppPaths::resolve(config_override)?;
    paths.ensure_layout()?;
    Ok(paths)
}

pub fn init_config(config_path: Option<PathBuf>) -> Result<PathBuf> {
    let paths = resolve_paths(config_path)?;
    let _ = AppConfig::load_or_create(&paths.config_path)?;
    Ok(paths.config_path)
}

pub fn setup(config_path: Option<PathBuf>) -> Result<()> {
    let paths = resolve_paths(config_path)?;
    log_info(&paths, "setup", "开始准备系统加速环境");
    let result = (|| -> Result<()> {
        let config = AppConfig::load_or_create(&paths.config_path)?;
        ensure_elevated(&config, true)?;
        ensure_loopback_alias(&config)?;
        let bundle = ensure_bundle(&config, &paths.cert_dir)?;
        install_ca(&bundle.ca_cert_path, &config.ca_common_name)?;
        apply_hosts(&config, &paths)?;
        let _ = flush_dns_cache();
        state::mark_stopped(&paths, "系统加速环境已准备")?;
        Ok(())
    })();
    match &result {
        Ok(_) => log_info(&paths, "setup", "系统加速环境已准备"),
        Err(error) => log_error(&paths, "setup", &format!("{error:#}")),
    }
    result
}

pub fn prepare_certificate(config_path: Option<PathBuf>) -> Result<()> {
    let paths = resolve_paths(config_path)?;
    log_info(&paths, "prepare-cert", "开始准备根证书");
    let result = (|| -> Result<()> {
        let config = AppConfig::load_or_create(&paths.config_path)?;
        let bundle = ensure_bundle(&config, &paths.cert_dir)?;
        install_ca(&bundle.ca_cert_path, &config.ca_common_name)?;
        Ok(())
    })();
    match &result {
        Ok(_) => log_info(&paths, "prepare-cert", "根证书已准备完成"),
        Err(error) => log_error(&paths, "prepare-cert", &format!("{error:#}")),
    }
    result
}

pub async fn run_foreground(config_path: Option<PathBuf>, with_setup: bool) -> Result<()> {
    let paths = resolve_paths(config_path)?;
    log_info(
        &paths,
        "daemon",
        if with_setup {
            "守护进程启动：包含环境准备"
        } else {
            "守护进程启动：直接进入前台代理"
        },
    );
    let config = AppConfig::load_or_create(&paths.config_path)?;
    ensure_elevated(&config, with_setup)?;
    ensure_loopback_alias(&config)?;
    let bundle = if with_setup {
        ensure_bundle(&config, &paths.cert_dir)?
    } else {
        load_or_create_bundle(&config, &paths.cert_dir)?
    };
    if with_setup {
        install_ca(&bundle.ca_cert_path, &config.ca_common_name)?;
        apply_hosts(&config, &paths)?;
        let _ = flush_dns_cache();
    }

    let pid = std::process::id();
    state::write_pid(&paths, pid)?;
    state::mark_running(&paths, pid)?;
    let ui_managed_shutdown = Arc::new(AtomicBool::new(false));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let watchdog = spawn_ui_lease_watchdog(paths.clone(), ui_managed_shutdown.clone(), shutdown_tx);
    let result = run_proxy(config.clone(), paths.clone(), bundle, shutdown_rx).await;
    watchdog.abort();
    let _ = state::clear_pid_if_matches(&paths, pid);
    if ui_managed_shutdown.load(Ordering::SeqCst) {
        let _ = cleanup_after_ui_disconnect(&paths, &config);
        log_info(
            &paths,
            "daemon",
            "已检测到前端异常退出，自动停止本地监听并恢复系统状态",
        );
        return Ok(());
    }
    match &result {
        Ok(_) => {
            log_info(&paths, "daemon", "加速服务已正常退出");
            let _ = state::mark_stopped(&paths, "加速服务已退出");
        }
        Err(error) => {
            log_error(&paths, "daemon", &format!("{error:#}"));
            let _ = state::mark_error(&paths, &error.to_string());
        }
    }
    result
}

pub fn helper_start(config_path: Option<PathBuf>) -> Result<()> {
    let paths = resolve_paths(config_path)?;
    log_info(&paths, "helper-start", "收到启动请求，开始准备加速环境");
    let config = AppConfig::load_or_create(&paths.config_path)?;
    let start_result = (|| -> Result<()> {
        ensure_elevated(&config, true)?;
        ensure_loopback_alias(&config)?;

        let current = reconcile_running_state(&paths, &config)?;
        if current.running {
            ensure_runtime_environment(&paths, &config)?;
            log_info(&paths, "helper-start", "检测到服务已在运行，跳过重复启动");
            return Ok(());
        }

        let bundle = ensure_bundle(&config, &paths.cert_dir)?;
        #[cfg(not(target_os = "macos"))]
        install_ca(&bundle.ca_cert_path, &config.ca_common_name)?;
        #[cfg(target_os = "macos")]
        let _ = bundle;
        apply_hosts(&config, &paths)?;
        let _ = flush_dns_cache();
        state::mark_starting(&paths)?;

        let cli_binary = current_cli_binary()?;
        let args = vec![
            "--config".to_string(),
            paths.config_path.to_string_lossy().into_owned(),
            "daemon".to_string(),
        ];
        let child_pid = spawn_detached(&cli_binary, &args)?;
        state::write_pid(&paths, child_pid)?;

        wait_until_running(&paths, &config, Duration::from_secs(10))?;
        thread::sleep(Duration::from_millis(800));
        let current = reconcile_running_state(&paths, &config)?;
        if !current.running {
            if let Some(error) = current.last_error {
                bail!(error);
            }
            bail!("accelerator daemon exited unexpectedly");
        }
        Ok(())
    })();

    match start_result {
        Ok(()) => {
            log_info(&paths, "helper-start", "加速服务启动成功");
            Ok(())
        }
        Err(error) => {
            if reconcile_running_state(&paths, &config)
                .map(|state| state.running)
                .unwrap_or(false)
            {
                log_warn(
                    &paths,
                    "helper-start",
                    &format!("启动流程报告异常，但检测到现有服务仍在运行，已修复状态：{error:#}"),
                );
                return Ok(());
            }

            let _ = remove_hosts(&paths);
            let _ = remove_loopback_alias(&config);
            let _ = state::clear_pid(&paths);
            let _ = state::mark_error(&paths, &format!("{error:#}"));
            log_error(&paths, "helper-start", &format!("{error:#}"));
            Err(error)
        }
    }
}

pub fn helper_stop(config_path: Option<PathBuf>) -> Result<()> {
    let paths = resolve_paths(config_path)?;
    log_info(&paths, "helper-stop", "收到停止请求，开始清理加速状态");
    let result = (|| -> Result<()> {
        let config = AppConfig::load_or_create(&paths.config_path)?;
        ensure_elevated(&config, true)?;
        if let Some(issue) = terminate_running_service(&paths)? {
            let _ = reconcile_running_state(&paths, &config);
            bail!(issue);
        }

        let mut issues = Vec::new();
        let (hosts_message, hosts_warning) = restore_hosts_after_stop(&paths);
        if let Some(warning) = hosts_warning {
            issues.push(warning);
        }
        if let Err(error) = remove_loopback_alias(&config) {
            issues.push(format!("failed to remove loopback alias: {error:#}"));
        }
        if let Err(error) = state::clear_pid(&paths) {
            issues.push(format!("failed to clear pid file: {error:#}"));
        }
        if let Err(error) = state::clear_ui_lease(&paths) {
            issues.push(format!("failed to clear ui lease: {error:#}"));
        }
        let _ = flush_dns_cache();

        let status_message = if issues.is_empty() {
            format!("加速已停止，{hosts_message}")
        } else {
            format!("已执行停止请求，但存在残留清理问题：{hosts_message}")
        };
        if let Err(error) = state::mark_stopped(&paths, &status_message) {
            issues.push(format!("failed to update service state: {error:#}"));
        }

        if issues.is_empty() {
            Ok(())
        } else {
            bail!(issues.join("; "));
        }
    })();
    match &result {
        Ok(_) => log_info(&paths, "helper-stop", "加速服务已停止并完成清理"),
        Err(error) => log_warn(&paths, "helper-stop", &format!("{error:#}")),
    }
    result
}

pub fn cleanup(config_path: Option<PathBuf>) -> Result<()> {
    let paths = resolve_paths(config_path)?;
    log_info(&paths, "cleanup", "收到彻底恢复请求，开始恢复系统修改");
    let result = (|| -> Result<()> {
        let config = AppConfig::load_or_create(&paths.config_path)?;
        ensure_elevated(&config, true)?;
        let mut issues = Vec::new();
        if let Some(issue) = terminate_running_service(&paths)? {
            issues.push(issue);
        }
        let (hosts_message, hosts_warning) = cleanup_hosts_state(&paths);
        if let Some(warning) = hosts_warning {
            issues.push(warning);
        }

        if let Err(error) = remove_loopback_alias(&config) {
            issues.push(format!("failed to remove loopback alias: {error:#}"));
        }
        if let Err(error) = uninstall_ca(&config.ca_common_name) {
            issues.push(format!("failed to uninstall certificate: {error:#}"));
        }
        if let Err(error) = state::clear_pid(&paths) {
            issues.push(format!("failed to clear pid file: {error:#}"));
        }
        if let Err(error) = state::clear_ui_lease(&paths) {
            issues.push(format!("failed to clear ui lease: {error:#}"));
        }

        let status_message = if issues.is_empty() {
            format!("已卸载证书并{hosts_message}")
        } else {
            format!("已执行 cleanup，但存在残留问题：{hosts_message}")
        };
        if let Err(error) = state::mark_stopped(&paths, &status_message) {
            issues.push(format!("failed to update service state: {error:#}"));
        }

        if issues.is_empty() {
            Ok(())
        } else {
            bail!(issues.join("; "));
        }
    })();
    match &result {
        Ok(_) => log_info(&paths, "cleanup", "已完成彻底恢复原始状态"),
        Err(error) => log_warn(&paths, "cleanup", &format!("{error:#}")),
    }
    result
}

pub fn clean_hosts(config_path: Option<PathBuf>) -> Result<()> {
    let paths = resolve_paths(config_path)?;
    log_info(&paths, "clean-hosts", "开始清理托管 hosts 规则");
    let result = (|| -> Result<()> {
        let config = AppConfig::load_or_create(&paths.config_path)?;
        ensure_elevated(&config, true)?;
        ensure_service_stopped_for_hosts_change(&paths, "clean-hosts")?;
        remove_hosts(&paths)?;
        remove_loopback_alias(&config)?;
        state::mark_stopped(&paths, "hosts 已清理")?;
        Ok(())
    })();
    match &result {
        Ok(_) => log_info(&paths, "clean-hosts", "托管 hosts 规则已清理"),
        Err(error) => log_error(&paths, "clean-hosts", &format!("{error:#}")),
    }
    result
}

pub fn apply_hosts_only(config_path: Option<PathBuf>) -> Result<()> {
    let paths = resolve_paths(config_path)?;
    log_info(&paths, "apply-hosts", "开始应用 hosts 接管规则");
    let result = (|| -> Result<()> {
        let config = AppConfig::load_or_create(&paths.config_path)?;
        ensure_elevated(&config, true)?;
        ensure_loopback_alias(&config)?;
        apply_hosts(&config, &paths)?;
        let _ = flush_dns_cache();
        Ok(())
    })();
    match &result {
        Ok(_) => log_info(&paths, "apply-hosts", "hosts 接管规则已应用"),
        Err(error) => log_error(&paths, "apply-hosts", &format!("{error:#}")),
    }
    result
}

pub fn backup_hosts(config_path: Option<PathBuf>) -> Result<()> {
    let paths = resolve_paths(config_path)?;
    log_info(&paths, "backup-hosts", "开始确保 hosts 基线备份存在");
    let result = backup_hosts_file(&paths);
    match &result {
        Ok(_) => log_info(&paths, "backup-hosts", "hosts 基线备份已就绪"),
        Err(error) => log_error(&paths, "backup-hosts", &format!("{error:#}")),
    }
    result
}

pub fn restore_hosts(config_path: Option<PathBuf>) -> Result<()> {
    let paths = resolve_paths(config_path)?;
    log_info(&paths, "restore-hosts", "开始恢复 hosts 备份");
    let result = (|| -> Result<()> {
        let config = AppConfig::load_or_create(&paths.config_path)?;
        ensure_elevated(&config, true)?;
        let was_running = state::refresh(&paths)?.running;
        if let Some(issue) = terminate_running_service(&paths)? {
            bail!(issue);
        }
        restore_hosts_file(&paths)?;
        let _ = flush_dns_cache();
        remove_loopback_alias(&config)?;
        let _ = state::clear_pid(&paths);
        state::mark_stopped(
            &paths,
            if was_running {
                "已停止加速并恢复 hosts 备份"
            } else {
                "hosts 已恢复为备份"
            },
        )?;
        Ok(())
    })();
    match &result {
        Ok(_) => log_info(&paths, "restore-hosts", "hosts 已恢复为备份"),
        Err(error) => log_error(&paths, "restore-hosts", &format!("{error:#}")),
    }
    result
}

pub fn uninstall_certificate(config_path: Option<PathBuf>) -> Result<()> {
    let paths = resolve_paths(config_path)?;
    log_info(&paths, "uninstall-cert", "开始卸载根证书");
    let result = (|| -> Result<()> {
        let config = AppConfig::load_or_create(&paths.config_path)?;
        ensure_elevated(&config, true)?;
        uninstall_ca(&config.ca_common_name)?;
        state::mark_stopped(&paths, "根证书已卸载")?;
        Ok(())
    })();
    match &result {
        Ok(_) => log_info(&paths, "uninstall-cert", "根证书已卸载"),
        Err(error) => log_error(&paths, "uninstall-cert", &format!("{error:#}")),
    }
    result
}

pub fn status(config_path: Option<PathBuf>) -> Result<state::ServiceState> {
    let paths = resolve_paths(config_path)?;
    let config = AppConfig::load_or_create(&paths.config_path)?;
    reconcile_running_state(&paths, &config)
}

fn reconcile_running_state(paths: &AppPaths, config: &AppConfig) -> Result<state::ServiceState> {
    let current = state::refresh(paths)?;
    if current.running {
        return Ok(current);
    }

    if let Some(pid) = discover_running_daemon_pid(paths) {
        state::write_pid(paths, pid)?;
        state::mark_running(paths, pid)?;
        return state::refresh(paths);
    }

    if proxy_ports_ready(config) {
        let repaired = state::ServiceState {
            running: true,
            pid: None,
            status_text: "加速中".to_string(),
            last_error: None,
            updated_at: now_ts(),
        };
        state::write(paths, &repaired)?;
        return Ok(repaired);
    }

    Ok(current)
}

fn ensure_runtime_environment(paths: &AppPaths, config: &AppConfig) -> Result<()> {
    ensure_loopback_alias(config)?;

    if !hosts_are_applied(config)? {
        apply_hosts(config, paths)?;
        let _ = flush_dns_cache();
        log_info(&paths, "helper-start", "已修复缺失的 hosts 接管规则");
    }

    Ok(())
}

fn wait_until_running(paths: &AppPaths, config: &AppConfig, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let state = reconcile_running_state(paths, config)?;
        if state.running {
            return Ok(());
        }
        if let Some(error) = state.last_error {
            bail!(error);
        }
        thread::sleep(Duration::from_millis(250));
    }

    bail!("daemon start timed out")
}

#[cfg_attr(not(target_family = "unix"), allow(unused_variables))]
fn discover_running_daemon_pid(paths: &AppPaths) -> Option<u32> {
    #[cfg(target_family = "unix")]
    {
        let output = Command::new("ps")
            .args(["ax", "-o", "pid=,command="])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }

        let config_marker = paths.config_path.to_string_lossy();
        let binary_marker = cli_binary_name();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let mut parts = trimmed.splitn(2, char::is_whitespace);
            let pid = parts.next()?.trim().parse::<u32>().ok()?;
            let command = parts.next().unwrap_or("").trim();
            if command.contains(binary_marker)
                && command.contains(" daemon")
                && command.contains(config_marker.as_ref())
                && is_process_running(pid)
            {
                return Some(pid);
            }
        }
    }

    None
}

fn proxy_ports_ready(config: &AppConfig) -> bool {
    tcp_port_ready(&config.listen_host, config.http_port)
        && tcp_port_ready(&config.listen_host, config.https_port)
}

fn tcp_port_ready(host: &str, port: u16) -> bool {
    let Ok(addresses) = format!("{host}:{port}").to_socket_addrs() else {
        return false;
    };

    addresses
        .into_iter()
        .any(|address| TcpStream::connect_timeout(&address, Duration::from_millis(250)).is_ok())
}

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn ensure_service_stopped_for_hosts_change(paths: &AppPaths, command_name: &str) -> Result<()> {
    let current = state::refresh(paths)?;
    if current.running {
        bail!("service is still running; stop or cleanup before `{command_name}`");
    }
    Ok(())
}

fn terminate_running_service(paths: &AppPaths) -> Result<Option<String>> {
    if let Some(pid) = state::read_pid(paths)? {
        if is_process_running(pid) {
            if let Err(error) = terminate_process(pid) {
                return Ok(Some(format!(
                    "failed to terminate running service pid {pid}: {error:#}"
                )));
            }
            if let Err(error) = wait_until_stopped(pid, Duration::from_secs(5)) {
                if let Err(force_error) = terminate_process_force(pid) {
                    return Ok(Some(format!(
                        "running service pid {pid} did not stop after SIGTERM ({error:#}); SIGKILL also failed: {force_error:#}"
                    )));
                }
                if let Err(force_wait_error) = wait_until_stopped(pid, Duration::from_secs(3)) {
                    return Ok(Some(format!(
                        "running service pid {pid} did not stop cleanly after SIGTERM ({error:#}) and SIGKILL ({force_wait_error:#})"
                    )));
                }
            }
        }
    }
    Ok(None)
}

fn cleanup_hosts_state(paths: &AppPaths) -> (String, Option<String>) {
    match backup_state(paths) {
        BackupState::Ready => match validate_hosts_backup_file(paths) {
            Ok(()) => match restore_hosts_file(paths) {
                Ok(()) => ("恢复原始 hosts".to_string(), None),
                Err(error) => match remove_hosts(paths) {
                    Ok(()) => (
                        "清理托管 hosts 规则（完整恢复失败）".to_string(),
                        Some(format!(
                            "failed to fully restore original hosts backup: {error:#}; cleanup fell back to removing the managed hosts block"
                        )),
                    ),
                    Err(remove_error) => (
                        "hosts 未清理".to_string(),
                        Some(format!(
                            "failed to fully restore original hosts backup: {error:#}; fallback removal also failed: {remove_error:#}"
                        )),
                    ),
                },
            },
            Err(validation_error) => match remove_hosts(paths) {
                Ok(()) => match clear_hosts_backup(paths) {
                    Ok(()) => ("清理托管 hosts 规则并重置损坏的备份状态".to_string(), None),
                    Err(clear_error) => (
                        "清理托管 hosts 规则（备份损坏）".to_string(),
                        Some(format!(
                            "hosts backup is invalid: {validation_error:#}; managed hosts block was removed but clearing stale backup artifacts failed: {clear_error:#}"
                        )),
                    ),
                },
                Err(remove_error) => (
                    "hosts 未清理".to_string(),
                    Some(format!(
                        "hosts backup is invalid: {validation_error:#}; fallback removal failed: {remove_error:#}"
                    )),
                ),
            },
        },
        BackupState::Missing => match remove_hosts(paths) {
            Ok(()) => ("清理托管 hosts 规则".to_string(), None),
            Err(error) => (
                "hosts 未清理".to_string(),
                Some(format!("failed to clean managed hosts block: {error:#}")),
            ),
        },
        BackupState::Inconsistent => match remove_hosts(paths) {
            Ok(()) => match clear_hosts_backup(paths) {
                Ok(()) => ("清理托管 hosts 规则并重置异常备份状态".to_string(), None),
                Err(clear_error) => (
                    "清理托管 hosts 规则（备份状态异常）".to_string(),
                    Some(format!(
                        "hosts backup state is inconsistent; managed hosts block was removed but clearing stale backup artifacts failed: {clear_error:#}"
                    )),
                ),
            },
            Err(error) => (
                "hosts 未清理".to_string(),
                Some(format!(
                    "hosts backup state is inconsistent; fallback removal failed: {error:#}"
                )),
            ),
        },
    }
}

fn restore_hosts_after_stop(paths: &AppPaths) -> (String, Option<String>) {
    match backup_state(paths) {
        BackupState::Ready => match validate_hosts_backup_file(paths) {
            Ok(()) => match restore_hosts_file(paths) {
                Ok(()) => ("hosts 已恢复为备份".to_string(), None),
                Err(error) => match remove_hosts(paths) {
                    Ok(()) => (
                        "hosts 备份恢复失败，已退化为仅清理托管 hosts 规则".to_string(),
                        Some(format!(
                            "failed to restore original hosts backup while stopping: {error:#}; fallback removal cleaned the managed hosts block"
                        )),
                    ),
                    Err(remove_error) => (
                        "hosts 未恢复".to_string(),
                        Some(format!(
                            "failed to restore original hosts backup while stopping: {error:#}; fallback removal also failed: {remove_error:#}"
                        )),
                    ),
                },
            },
            Err(validation_error) => match remove_hosts(paths) {
                Ok(()) => (
                    "hosts 备份异常，已退化为仅清理托管 hosts 规则".to_string(),
                    Some(format!(
                        "hosts backup is invalid while stopping: {validation_error:#}; managed hosts block was removed instead"
                    )),
                ),
                Err(remove_error) => (
                    "hosts 未恢复".to_string(),
                    Some(format!(
                        "hosts backup is invalid while stopping: {validation_error:#}; fallback removal failed: {remove_error:#}"
                    )),
                ),
            },
        },
        BackupState::Missing => match remove_hosts(paths) {
            Ok(()) => ("已清理托管 hosts 规则".to_string(), None),
            Err(error) => (
                "hosts 未恢复".to_string(),
                Some(format!("failed to clean managed hosts block while stopping: {error:#}")),
            ),
        },
        BackupState::Inconsistent => match remove_hosts(paths) {
            Ok(()) => (
                "hosts 备份状态异常，已退化为仅清理托管 hosts 规则".to_string(),
                Some("hosts backup state is inconsistent while stopping; removed only the managed hosts block".to_string()),
            ),
            Err(error) => (
                "hosts 未恢复".to_string(),
                Some(format!(
                    "hosts backup state is inconsistent while stopping; fallback removal failed: {error:#}"
                )),
            ),
        },
    }
}

fn wait_until_stopped(pid: u32, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !is_process_running(pid) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(200));
    }

    bail!("process {pid} did not stop within {:?}", timeout)
}

fn cleanup_after_ui_disconnect(paths: &AppPaths, config: &AppConfig) -> Result<()> {
    let mut issues = Vec::new();
    let (hosts_message, hosts_warning) = restore_hosts_after_stop(paths);
    if let Some(warning) = hosts_warning {
        issues.push(warning);
    }
    if let Err(error) = remove_loopback_alias(config) {
        issues.push(format!("failed to remove loopback alias: {error:#}"));
    }
    if let Err(error) = state::clear_pid(paths) {
        issues.push(format!("failed to clear pid file: {error:#}"));
    }
    if let Err(error) = state::clear_ui_lease(paths) {
        issues.push(format!("failed to clear ui lease: {error:#}"));
    }
    let _ = flush_dns_cache();

    let status_message = if issues.is_empty() {
        format!("前端已退出，自动停止加速并{hosts_message}")
    } else {
        format!("前端已退出，已停止监听，但存在残留清理问题：{hosts_message}")
    };
    let _ = state::mark_stopped(paths, &status_message);

    if issues.is_empty() {
        Ok(())
    } else {
        bail!(issues.join("; "));
    }
}

fn spawn_ui_lease_watchdog(
    paths: AppPaths,
    ui_managed_shutdown: Arc<AtomicBool>,
    shutdown_tx: watch::Sender<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        match state::read_ui_lease(&paths) {
            Ok(Some(_)) => {}
            Ok(None) => return,
            Err(error) => {
                let _ = runtime_log::append(
                    &paths,
                    "WARN",
                    "ui-watchdog",
                    &format!("failed to read initial ui lease state: {error:#}"),
                );
                return;
            }
        }
        let stale_after = Duration::from_secs(8);
        let mut missing_since: Option<u64> = None;
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;

            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .unwrap_or(0);
            let lease = match state::read_ui_lease(&paths) {
                Ok(Some(lease)) => lease,
                Ok(None) => {
                    let first_missing_at = *missing_since.get_or_insert(now);
                    let missing_for = now.saturating_sub(first_missing_at);
                    if missing_for < stale_after.as_secs() {
                        continue;
                    }
                    ui_managed_shutdown.store(true, Ordering::SeqCst);
                    let _ = runtime_log::append(
                        &paths,
                        "WARN",
                        "ui-watchdog",
                        &format!(
                            "ui lease missing for {}s while daemon is ui-managed; requesting shutdown",
                            missing_for
                        ),
                    );
                    let _ = shutdown_tx.send(true);
                    break;
                }
                Err(error) => {
                    let _ = runtime_log::append(
                        &paths,
                        "WARN",
                        "ui-watchdog",
                        &format!("failed to read ui lease state, keeping daemon alive: {error:#}"),
                    );
                    continue;
                }
            };
            missing_since = None;

            let now = now.max(lease.updated_at);
            let stale = now.saturating_sub(lease.updated_at) >= stale_after.as_secs();
            if stale {
                let owner_dead = !is_process_running(lease.owner_pid);
                ui_managed_shutdown.store(true, Ordering::SeqCst);
                let _ = runtime_log::append(
                    &paths,
                    "WARN",
                    "ui-watchdog",
                    &format!(
                        "ui lease expired owner_pid={} stale={} owner_dead={}; requesting shutdown",
                        lease.owner_pid, stale, owner_dead
                    ),
                );
                let _ = shutdown_tx.send(true);
                break;
            }
        }
    })
}

fn log_info(paths: &AppPaths, action: &str, message: &str) {
    let _ = runtime_log::append(paths, "INFO", action, message);
}

fn log_warn(paths: &AppPaths, action: &str, message: &str) {
    let _ = runtime_log::append(paths, "WARN", action, message);
}

fn log_error(paths: &AppPaths, action: &str, message: &str) {
    let _ = runtime_log::append(paths, "ERROR", action, message);
}

fn current_cli_binary() -> Result<PathBuf> {
    if let Ok(path) = std::env::current_exe() {
        let sibling = path.with_file_name(cli_binary_name());
        if sibling.exists() {
            return Ok(sibling);
        }
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name == cli_binary_name())
            .unwrap_or(false)
        {
            return Ok(path);
        }
    }

    bail!("failed to locate CLI binary")
}

fn cli_binary_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "linuxdo-accelerator.exe"
    } else {
        "linuxdo-accelerator"
    }
}
