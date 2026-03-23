use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::certs::{ensure_bundle, load_or_create_bundle};
use crate::config::AppConfig;
use crate::hosts::{apply_hosts, remove_hosts};
use crate::paths::AppPaths;
use crate::platform::{
    ensure_elevated, ensure_loopback_alias, install_ca, is_process_running, remove_loopback_alias,
    spawn_detached, terminate_process, uninstall_ca,
};
use crate::proxy::run_proxy;
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
    let config = AppConfig::load_or_create(&paths.config_path)?;
    ensure_elevated(&config, true)?;
    ensure_loopback_alias(&config)?;
    let bundle = ensure_bundle(&config, &paths.cert_dir)?;
    install_ca(&bundle.ca_cert_path, &config.ca_common_name)?;
    apply_hosts(&config, &paths)?;
    state::mark_stopped(&paths, "系统加速环境已准备")?;
    Ok(())
}

pub fn prepare_certificate(config_path: Option<PathBuf>) -> Result<()> {
    let paths = resolve_paths(config_path)?;
    let config = AppConfig::load_or_create(&paths.config_path)?;
    let bundle = ensure_bundle(&config, &paths.cert_dir)?;
    install_ca(&bundle.ca_cert_path, &config.ca_common_name)?;
    Ok(())
}

pub async fn run_foreground(config_path: Option<PathBuf>, with_setup: bool) -> Result<()> {
    let paths = resolve_paths(config_path)?;
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
    }

    let pid = std::process::id();
    state::write_pid(&paths, pid)?;
    state::mark_running(&paths, pid)?;
    let result = run_proxy(config, bundle).await;
    let _ = state::clear_pid(&paths);
    match &result {
        Ok(_) => {
            let _ = state::mark_stopped(&paths, "加速服务已退出");
        }
        Err(error) => {
            let _ = state::mark_error(&paths, &error.to_string());
        }
    }
    result
}

pub fn helper_start(config_path: Option<PathBuf>) -> Result<()> {
    let paths = resolve_paths(config_path)?;
    let start_result = (|| -> Result<()> {
        let config = AppConfig::load_or_create(&paths.config_path)?;
        ensure_elevated(&config, true)?;
        ensure_loopback_alias(&config)?;

        let current = state::refresh(&paths)?;
        if current.running {
            return Ok(());
        }

        let bundle = ensure_bundle(&config, &paths.cert_dir)?;
        #[cfg(not(target_os = "macos"))]
        install_ca(&bundle.ca_cert_path, &config.ca_common_name)?;
        #[cfg(target_os = "macos")]
        let _ = bundle;
        apply_hosts(&config, &paths)?;
        state::mark_starting(&paths)?;

        let cli_binary = current_cli_binary()?;
        let args = vec![
            "--config".to_string(),
            paths.config_path.to_string_lossy().into_owned(),
            "daemon".to_string(),
        ];
        let child_pid = spawn_detached(&cli_binary, &args)?;
        state::write_pid(&paths, child_pid)?;

        wait_until_running(&paths, Duration::from_secs(10))?;
        thread::sleep(Duration::from_millis(800));
        let current = state::refresh(&paths)?;
        if !current.running {
            if let Some(error) = current.last_error {
                bail!(error);
            }
            bail!("accelerator daemon exited unexpectedly");
        }
        Ok(())
    })();

    if let Err(error) = &start_result {
        let _ = remove_hosts(&paths);
        let _ = state::clear_pid(&paths);
        let _ = state::mark_error(&paths, &format!("{error:#}"));
    }

    start_result
}

pub fn helper_stop(config_path: Option<PathBuf>) -> Result<()> {
    let paths = resolve_paths(config_path)?;
    let config = AppConfig::load_or_create(&paths.config_path)?;
    ensure_elevated(&config, true)?;

    if let Some(pid) = state::read_pid(&paths)? {
        if is_process_running(pid) {
            terminate_process(pid)?;
            wait_until_stopped(pid, Duration::from_secs(5));
        }
    }

    remove_hosts(&paths)?;
    remove_loopback_alias(&config)?;
    state::clear_pid(&paths)?;
    state::mark_stopped(&paths, "加速已停止")?;
    Ok(())
}

pub fn cleanup(config_path: Option<PathBuf>) -> Result<()> {
    let paths = resolve_paths(config_path)?;
    let config = AppConfig::load_or_create(&paths.config_path)?;
    ensure_elevated(&config, true)?;
    let _ = helper_stop(Some(paths.config_path.clone()));
    uninstall_ca(&config.ca_common_name)?;
    state::mark_stopped(&paths, "已清理证书和 hosts")?;
    Ok(())
}

pub fn clean_hosts(config_path: Option<PathBuf>) -> Result<()> {
    let paths = resolve_paths(config_path)?;
    let config = AppConfig::load_or_create(&paths.config_path)?;
    ensure_elevated(&config, true)?;
    remove_hosts(&paths)?;
    remove_loopback_alias(&config)?;
    state::mark_stopped(&paths, "hosts 已清理")?;
    Ok(())
}

pub fn apply_hosts_only(config_path: Option<PathBuf>) -> Result<()> {
    let paths = resolve_paths(config_path)?;
    let config = AppConfig::load_or_create(&paths.config_path)?;
    ensure_elevated(&config, true)?;
    ensure_loopback_alias(&config)?;
    apply_hosts(&config, &paths)?;
    Ok(())
}

pub fn uninstall_certificate(config_path: Option<PathBuf>) -> Result<()> {
    let paths = resolve_paths(config_path)?;
    let config = AppConfig::load_or_create(&paths.config_path)?;
    ensure_elevated(&config, true)?;
    uninstall_ca(&config.ca_common_name)?;
    state::mark_stopped(&paths, "根证书已卸载")?;
    Ok(())
}

pub fn status(config_path: Option<PathBuf>) -> Result<state::ServiceState> {
    let paths = resolve_paths(config_path)?;
    state::refresh(&paths)
}

fn wait_until_running(paths: &AppPaths, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let state = state::refresh(paths)?;
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

fn wait_until_stopped(pid: u32, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !is_process_running(pid) {
            break;
        }
        thread::sleep(Duration::from_millis(200));
    }
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
