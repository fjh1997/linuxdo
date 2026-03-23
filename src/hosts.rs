use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::config::AppConfig;
use crate::paths::AppPaths;

const START_MARKER: &str = "# >>> linuxdo-accelerator >>>";
const END_MARKER: &str = "# <<< linuxdo-accelerator <<<";

pub fn apply_hosts(config: &AppConfig, _paths: &AppPaths) -> Result<()> {
    #[cfg(target_os = "android")]
    {
        return apply_android_hosts(config, _paths);
    }

    #[cfg(not(target_os = "android"))]
    {
        let path = hosts_path();
        let original = fs::read_to_string(&path)
            .with_context(|| format!("failed to read hosts file {}", path.display()))?;
        let content = render_managed_hosts(&original, config);

        fs::write(&path, content)
            .with_context(|| format!("failed to update hosts file {}", path.display()))?;
        Ok(())
    }
}

pub fn remove_hosts(_paths: &AppPaths) -> Result<()> {
    #[cfg(target_os = "android")]
    {
        return remove_android_hosts(_paths);
    }

    #[cfg(not(target_os = "android"))]
    {
        let path = hosts_path();
        let original = fs::read_to_string(&path)
            .with_context(|| format!("failed to read hosts file {}", path.display()))?;
        let stripped = strip_managed_block(&original);
        fs::write(&path, stripped)
            .with_context(|| format!("failed to clean hosts file {}", path.display()))?;
        Ok(())
    }
}

#[cfg(not(target_os = "android"))]
fn render_managed_hosts(original: &str, config: &AppConfig) -> String {
    let newline = detect_newline(original);
    let mut content = strip_managed_block(original);

    if !content.is_empty() && !content.ends_with(newline) {
        content.push_str(newline);
    }

    content.push_str(START_MARKER);
    content.push_str(newline);
    for host in config.hosts_domains() {
        if host.starts_with("*.") {
            continue;
        }
        content.push_str(&format!("{} {}", config.hosts_ip, host));
        content.push_str(newline);
    }
    content.push_str(END_MARKER);
    content.push_str(newline);
    content
}

#[cfg(target_os = "android")]
fn apply_android_hosts(_config: &AppConfig, paths: &AppPaths) -> Result<()> {
    let marker_path = android_hosts_marker_path(paths);
    if let Some(parent) = marker_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(
        &marker_path,
        "android uses internal dns_hosts overrides; system hosts is not touched\n",
    )
    .with_context(|| format!("failed to write {}", marker_path.display()))?;
    Ok(())
}

#[cfg(target_os = "android")]
fn remove_android_hosts(paths: &AppPaths) -> Result<()> {
    let marker_path = android_hosts_marker_path(paths);
    if marker_path.exists() {
        fs::remove_file(&marker_path)
            .with_context(|| format!("failed to remove {}", marker_path.display()))?;
    }
    Ok(())
}

#[cfg(target_os = "android")]
fn android_hosts_marker_path(paths: &AppPaths) -> PathBuf {
    paths.runtime_dir.join("android-dns-hosts.txt")
}

#[cfg(not(target_os = "android"))]
fn detect_newline(content: &str) -> &'static str {
    if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

#[cfg(not(target_os = "android"))]
fn strip_managed_block(content: &str) -> String {
    let mut output = Vec::new();
    let mut skipping = false;
    for line in content.lines() {
        if line.trim() == START_MARKER {
            skipping = true;
            continue;
        }
        if line.trim() == END_MARKER {
            skipping = false;
            continue;
        }
        if !skipping {
            output.push(line);
        }
    }

    output.join(if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    })
}

#[cfg(not(target_os = "android"))]
fn hosts_path() -> PathBuf {
    if cfg!(target_os = "windows") {
        PathBuf::from(r"C:\Windows\System32\drivers\etc\hosts")
    } else {
        PathBuf::from("/etc/hosts")
    }
}

#[cfg(target_os = "android")]
fn hosts_path() -> PathBuf {
    if cfg!(target_os = "android") {
        PathBuf::from("/system/etc/hosts")
    } else {
        unreachable!()
    }
}
