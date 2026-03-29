use std::fs;
#[cfg(target_os = "windows")]
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::AppPaths;
use crate::platform::is_process_running;
use crate::platform::sync_user_ownership;
#[cfg(target_os = "windows")]
use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MoveFileExW};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceState {
    pub running: bool,
    pub pid: Option<u32>,
    pub status_text: String,
    pub last_error: Option<String>,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiLeaseState {
    pub owner_pid: u32,
    pub updated_at: u64,
}

impl Default for ServiceState {
    fn default() -> Self {
        Self {
            running: false,
            pid: None,
            status_text: "未启动".to_string(),
            last_error: None,
            updated_at: now_ts(),
        }
    }
}

pub fn read(paths: &AppPaths) -> Result<ServiceState> {
    if !paths.state_path.exists() {
        return Ok(ServiceState::default());
    }

    let content = fs::read_to_string(&paths.state_path)
        .with_context(|| format!("failed to read {}", paths.state_path.display()))?;
    let state = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", paths.state_path.display()))?;
    Ok(state)
}

pub fn refresh(paths: &AppPaths) -> Result<ServiceState> {
    let mut state = read(paths)?;
    let live_pid = read_pid(paths)?.filter(|pid| is_process_running(*pid));

    if let Some(pid) = live_pid {
        let mut changed = false;
        if !state.running {
            state.running = true;
            changed = true;
        }
        if state.pid != Some(pid) {
            state.pid = Some(pid);
            changed = true;
        }
        if state.last_error.is_some() {
            state.last_error = None;
            changed = true;
        }
        let expected = format!("加速中，PID {pid}");
        if state.status_text != expected {
            state.status_text = expected;
            changed = true;
        }
        if changed {
            state.updated_at = now_ts();
            let _ = write(paths, &state);
        }
        return Ok(state);
    }

    if state.running || state.pid.is_some() {
        state.running = false;
        state.pid = None;
        if state.last_error.is_none() {
            state.status_text = "已停止".to_string();
        }
        state.updated_at = now_ts();
        let _ = clear_pid(paths);
        let _ = write(paths, &state);
    }
    Ok(state)
}

pub fn write(paths: &AppPaths, state: &ServiceState) -> Result<()> {
    let content =
        serde_json::to_string_pretty(state).context("failed to serialize service state")?;
    replace_file(&paths.state_path, content.as_bytes())?;
    sync_user_ownership(&paths.state_path)?;
    Ok(())
}

pub fn mark_running(paths: &AppPaths, pid: u32) -> Result<()> {
    let state = ServiceState {
        running: true,
        pid: Some(pid),
        status_text: format!("加速中，PID {pid}"),
        last_error: None,
        updated_at: now_ts(),
    };
    write(paths, &state)
}

pub fn mark_starting(paths: &AppPaths) -> Result<()> {
    let state = ServiceState {
        status_text: "正在启动加速服务...".to_string(),
        updated_at: now_ts(),
        ..ServiceState::default()
    };
    write(paths, &state)
}

pub fn mark_stopped(paths: &AppPaths, message: &str) -> Result<()> {
    let state = ServiceState {
        running: false,
        pid: None,
        status_text: message.to_string(),
        last_error: None,
        updated_at: now_ts(),
    };
    write(paths, &state)
}

pub fn mark_error(paths: &AppPaths, message: &str) -> Result<()> {
    let state = ServiceState {
        running: false,
        pid: None,
        status_text: "启动失败".to_string(),
        last_error: Some(message.to_string()),
        updated_at: now_ts(),
    };
    write(paths, &state)
}

pub fn write_pid(paths: &AppPaths, pid: u32) -> Result<()> {
    replace_file(&paths.pid_path, pid.to_string().as_bytes())?;
    sync_user_ownership(&paths.pid_path)?;
    Ok(())
}

pub fn read_pid(paths: &AppPaths) -> Result<Option<u32>> {
    if !paths.pid_path.exists() {
        return Ok(None);
    }

    let value = fs::read_to_string(&paths.pid_path)
        .with_context(|| format!("failed to read {}", paths.pid_path.display()))?;
    let pid = value.trim().parse::<u32>().ok();
    Ok(pid)
}

pub fn clear_pid_if_matches(paths: &AppPaths, expected_pid: u32) -> Result<bool> {
    if read_pid(paths)? != Some(expected_pid) {
        return Ok(false);
    }

    clear_pid(paths)?;
    Ok(true)
}

pub fn clear_pid(paths: &AppPaths) -> Result<()> {
    if paths.pid_path.exists() {
        fs::remove_file(&paths.pid_path)
            .with_context(|| format!("failed to remove {}", paths.pid_path.display()))?;
    }
    Ok(())
}

pub fn read_ui_lease(paths: &AppPaths) -> Result<Option<UiLeaseState>> {
    if !paths.ui_lease_path.exists() {
        return Ok(None);
    }

    let content = fs::read_to_string(&paths.ui_lease_path)
        .with_context(|| format!("failed to read {}", paths.ui_lease_path.display()))?;
    let lease = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", paths.ui_lease_path.display()))?;
    Ok(Some(lease))
}

pub fn touch_ui_lease(paths: &AppPaths, owner_pid: u32) -> Result<()> {
    let lease = UiLeaseState {
        owner_pid,
        updated_at: now_ts(),
    };
    let content =
        serde_json::to_string_pretty(&lease).context("failed to serialize ui lease state")?;
    replace_file(&paths.ui_lease_path, content.as_bytes())?;
    sync_user_ownership(&paths.ui_lease_path)?;
    Ok(())
}

pub fn clear_ui_lease(paths: &AppPaths) -> Result<()> {
    if paths.ui_lease_path.exists() {
        fs::remove_file(&paths.ui_lease_path)
            .with_context(|| format!("failed to remove {}", paths.ui_lease_path.display()))?;
    }
    Ok(())
}

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn replace_file(path: &Path, content: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("state");
    let tmp_path = path.with_file_name(format!(".{file_name}.tmp-{}", std::process::id()));

    fs::write(&tmp_path, content)
        .with_context(|| format!("failed to write {}", tmp_path.display()))?;

    move_file_replace(&tmp_path, path).with_context(|| {
        format!("failed to move {} to {}", tmp_path.display(), path.display())
    })?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn move_file_replace(src: &Path, dst: &Path) -> Result<()> {
    let src_wide: Vec<u16> = src.as_os_str().encode_wide().chain(Some(0)).collect();
    let dst_wide: Vec<u16> = dst.as_os_str().encode_wide().chain(Some(0)).collect();
    let ok = unsafe { MoveFileExW(src_wide.as_ptr(), dst_wide.as_ptr(), MOVEFILE_REPLACE_EXISTING) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error()).context("MoveFileExW failed");
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn move_file_replace(src: &Path, dst: &Path) -> Result<()> {
    fs::rename(src, dst).context("rename failed")
}
