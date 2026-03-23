use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use directories::ProjectDirs;

use crate::platform::sync_user_ownership;

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub config_path: PathBuf,
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub cert_dir: PathBuf,
    pub state_path: PathBuf,
    pub pid_path: PathBuf,
}

impl AppPaths {
    pub fn resolve(config_override: Option<PathBuf>) -> Result<Self> {
        let (default_config_dir, default_data_dir) = default_app_dirs()?;
        let has_config_override = config_override.is_some();
        let config_path =
            config_override.unwrap_or_else(|| default_config_dir.join("linuxdo-accelerator.toml"));
        let config_dir = config_path
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| anyhow!("invalid config path {}", config_path.display()))?;

        let data_dir = if has_config_override {
            if config_dir
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.eq_ignore_ascii_case("config"))
                .unwrap_or(false)
            {
                config_dir
                    .parent()
                    .map(|root| root.join("data"))
                    .unwrap_or_else(|| default_data_dir.clone())
            } else {
                config_dir.clone()
            }
        } else {
            default_data_dir
        };
        let runtime_dir = data_dir.join("runtime");
        let cert_dir = data_dir.join("certs");
        let state_path = runtime_dir.join("service-state.json");
        let pid_path = runtime_dir.join("linuxdo-accelerator.pid");

        Ok(Self {
            config_path,
            config_dir,
            data_dir,
            runtime_dir,
            cert_dir,
            state_path,
            pid_path,
        })
    }

    pub fn ensure_layout(&self) -> Result<()> {
        fs::create_dir_all(&self.config_dir)
            .with_context(|| format!("failed to create {}", self.config_dir.display()))?;
        fs::create_dir_all(&self.data_dir)
            .with_context(|| format!("failed to create {}", self.data_dir.display()))?;
        fs::create_dir_all(&self.runtime_dir)
            .with_context(|| format!("failed to create {}", self.runtime_dir.display()))?;
        fs::create_dir_all(&self.cert_dir)
            .with_context(|| format!("failed to create {}", self.cert_dir.display()))?;
        sync_user_ownership(&self.config_dir)?;
        sync_user_ownership(&self.data_dir)?;
        Ok(())
    }
}

fn default_app_dirs() -> Result<(PathBuf, PathBuf)> {
    #[cfg(target_os = "android")]
    {
        let root = PathBuf::from("/data/local/tmp/linuxdo-accelerator");
        return Ok((root.join("config"), root.join("data")));
    }

    #[cfg(target_os = "linux")]
    {
        if let Some(home) = effective_linux_home_dir() {
            return Ok((
                home.join(".config").join("linuxdo-accelerator"),
                home.join(".local")
                    .join("share")
                    .join("linuxdo-accelerator"),
            ));
        }
    }

    let dirs = ProjectDirs::from("io", "linuxdo", "linuxdo-accelerator")
        .ok_or_else(|| anyhow!("failed to resolve platform application directories"))?;
    Ok((
        dirs.config_dir().to_path_buf(),
        dirs.data_local_dir().to_path_buf(),
    ))
}

#[cfg(target_os = "linux")]
fn effective_linux_home_dir() -> Option<PathBuf> {
    let uid = std::env::var("PKEXEC_UID")
        .ok()
        .or_else(|| std::env::var("SUDO_UID").ok())
        .and_then(|value| value.parse::<u32>().ok());

    if let Some(uid) = uid
        && let Some(home) = home_dir_from_uid(uid)
    {
        return Some(home);
    }

    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(target_os = "linux")]
fn home_dir_from_uid(uid: u32) -> Option<PathBuf> {
    unsafe {
        let passwd = libc::getpwuid(uid);
        if passwd.is_null() {
            return None;
        }
        let home = std::ffi::CStr::from_ptr((*passwd).pw_dir);
        Some(PathBuf::from(home.to_string_lossy().into_owned()))
    }
}
