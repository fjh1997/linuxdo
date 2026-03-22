use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::config::AppConfig;

const START_MARKER: &str = "# >>> linuxdo-accelerator >>>";
const END_MARKER: &str = "# <<< linuxdo-accelerator <<<";

pub fn apply_hosts(config: &AppConfig) -> Result<()> {
    let path = hosts_path();
    let original = fs::read_to_string(&path)
        .with_context(|| format!("failed to read hosts file {}", path.display()))?;
    let newline = detect_newline(&original);
    let mut content = strip_managed_block(&original);

    if !content.is_empty() && !content.ends_with(newline) {
        content.push_str(newline);
    }

    content.push_str(START_MARKER);
    content.push_str(newline);
    for host in &config.proxy_domains {
        if host.starts_with("*.") {
            continue;
        }
        content.push_str(&format!("{} {}", config.hosts_ip, host));
        content.push_str(newline);
    }
    content.push_str(END_MARKER);
    content.push_str(newline);

    fs::write(&path, content)
        .with_context(|| format!("failed to update hosts file {}", path.display()))?;
    Ok(())
}

pub fn remove_hosts() -> Result<()> {
    let path = hosts_path();
    let original = fs::read_to_string(&path)
        .with_context(|| format!("failed to read hosts file {}", path.display()))?;
    let stripped = strip_managed_block(&original);
    fs::write(&path, stripped)
        .with_context(|| format!("failed to clean hosts file {}", path.display()))?;
    Ok(())
}

fn detect_newline(content: &str) -> &'static str {
    if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

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

fn hosts_path() -> PathBuf {
    if cfg!(target_os = "windows") {
        PathBuf::from(r"C:\Windows\System32\drivers\etc\hosts")
    } else {
        PathBuf::from("/etc/hosts")
    }
}
