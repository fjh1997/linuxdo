use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::AppPaths;
use crate::platform::is_process_running;
use crate::platform::sync_user_ownership;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceState {
    pub running: bool,
    pub pid: Option<u32>,
    pub status_text: String,
    pub last_error: Option<String>,
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
        let expected = format!("加速中，PID {pid}");
        if state.last_error.is_none() && state.status_text != expected {
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
    fs::write(&paths.state_path, content)
        .with_context(|| format!("failed to write {}", paths.state_path.display()))?;
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
    fs::write(&paths.pid_path, pid.to_string())
        .with_context(|| format!("failed to write {}", paths.pid_path.display()))?;
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

pub fn clear_pid(paths: &AppPaths) -> Result<()> {
    if paths.pid_path.exists() {
        fs::remove_file(&paths.pid_path)
            .with_context(|| format!("failed to remove {}", paths.pid_path.display()))?;
    }
    Ok(())
}

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}
