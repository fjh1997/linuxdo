use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

const DEFAULT_APP_CONFIG: &str = include_str!("../assets/defaults/linuxdo-accelerator.toml");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub listen_host: String,
    pub hosts_ip: String,
    pub http_port: u16,
    pub https_port: u16,
    pub upstream: String,
    #[serde(default = "default_fake_sni")]
    pub fake_sni: Option<String>,
    pub doh_endpoints: Vec<String>,
    pub proxy_domains: Vec<String>,
    pub certificate_domains: Vec<String>,
    pub ca_common_name: String,
    pub server_common_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacyMainConfig {
    pub listen_host: String,
    pub hosts_ip: String,
    pub http_port: u16,
    pub https_port: u16,
    pub upstream: String,
    #[serde(default = "default_fake_sni")]
    pub fake_sni: Option<String>,
    pub ca_common_name: String,
    pub server_common_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacyNetworkProfile {
    pub doh_endpoints: Vec<String>,
    pub proxy_domains: Vec<String>,
    pub certificate_domains: Vec<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        default_app_config()
    }
}

fn default_fake_sni() -> Option<String> {
    Some("www.cloudflare.com".to_string())
}

fn default_app_config() -> AppConfig {
    toml::from_str(DEFAULT_APP_CONFIG)
        .expect("assets/defaults/linuxdo-accelerator.toml must be valid TOML")
}

fn legacy_network_profile_path(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .map(|parent| parent.join("linuxdo-network.toml"))
        .unwrap_or_else(|| PathBuf::from("linuxdo-network.toml"))
}

impl AppConfig {
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        if !path.exists() {
            fs::write(path, DEFAULT_APP_CONFIG)
                .with_context(|| format!("failed to write config {}", path.display()))?;
            cleanup_legacy_network_profile(path)?;
            return Ok(default_app_config());
        }

        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;

        if let Ok(mut config) = toml::from_str::<AppConfig>(&content) {
            let defaults = default_app_config();
            let mut changed = false;
            changed |= merge_missing_values(&mut config.doh_endpoints, defaults.doh_endpoints);
            changed |= merge_missing_values(&mut config.proxy_domains, defaults.proxy_domains);
            changed |= merge_missing_values(
                &mut config.certificate_domains,
                defaults.certificate_domains,
            );

            let serialized =
                toml::to_string_pretty(&config).context("failed to serialize config")?;
            if changed || normalize_toml(&content) != normalize_toml(&serialized) {
                fs::write(path, serialized)
                    .with_context(|| format!("failed to rewrite config {}", path.display()))?;
            }
            cleanup_legacy_network_profile(path)?;
            return Ok(config);
        }

        let legacy: LegacyMainConfig = toml::from_str(&content)
            .with_context(|| format!("failed to parse config {}", path.display()))?;
        let legacy_network = load_legacy_network_profile(path)?;
        let defaults = default_app_config();
        let mut config = AppConfig {
            listen_host: legacy.listen_host,
            hosts_ip: legacy.hosts_ip,
            http_port: legacy.http_port,
            https_port: legacy.https_port,
            upstream: legacy.upstream,
            fake_sni: legacy.fake_sni,
            doh_endpoints: legacy_network
                .as_ref()
                .map(|value| value.doh_endpoints.clone())
                .unwrap_or_default(),
            proxy_domains: legacy_network
                .as_ref()
                .map(|value| value.proxy_domains.clone())
                .unwrap_or_default(),
            certificate_domains: legacy_network
                .as_ref()
                .map(|value| value.certificate_domains.clone())
                .unwrap_or_default(),
            ca_common_name: legacy.ca_common_name,
            server_common_name: legacy.server_common_name,
        };
        merge_missing_values(&mut config.doh_endpoints, defaults.doh_endpoints);
        merge_missing_values(&mut config.proxy_domains, defaults.proxy_domains);
        merge_missing_values(
            &mut config.certificate_domains,
            defaults.certificate_domains,
        );

        let serialized =
            toml::to_string_pretty(&config).context("failed to serialize migrated config")?;
        fs::write(path, serialized)
            .with_context(|| format!("failed to rewrite config {}", path.display()))?;
        cleanup_legacy_network_profile(path)?;
        Ok(config)
    }

    pub fn save_default(path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        if !path.exists() {
            fs::write(path, DEFAULT_APP_CONFIG)
                .with_context(|| format!("failed to write config {}", path.display()))?;
        }
        cleanup_legacy_network_profile(path)?;
        Ok(())
    }

    pub fn matches_proxy_host(&self, host: &str) -> bool {
        let candidate = host.to_ascii_lowercase();
        self.proxy_domains.iter().any(|pattern| {
            let pattern = pattern.to_ascii_lowercase();
            if let Some(suffix) = pattern.strip_prefix("*.") {
                return candidate.ends_with(&format!(".{suffix}"));
            }
            candidate == pattern
        })
    }
}

fn load_legacy_network_profile(path: &Path) -> Result<Option<LegacyNetworkProfile>> {
    let legacy_path = legacy_network_profile_path(path);
    if !legacy_path.exists() {
        return Ok(None);
    }

    let content = fs::read_to_string(&legacy_path)
        .with_context(|| format!("failed to read {}", legacy_path.display()))?;
    let profile: LegacyNetworkProfile = toml::from_str(&content)
        .with_context(|| format!("failed to parse {}", legacy_path.display()))?;

    if profile.doh_endpoints.is_empty() {
        return Err(anyhow!("legacy network profile is missing doh_endpoints"));
    }
    if profile.proxy_domains.is_empty() {
        return Err(anyhow!("legacy network profile is missing proxy_domains"));
    }
    if profile.certificate_domains.is_empty() {
        return Err(anyhow!(
            "legacy network profile is missing certificate_domains"
        ));
    }

    Ok(Some(profile))
}

fn cleanup_legacy_network_profile(path: &Path) -> Result<()> {
    let legacy_path = legacy_network_profile_path(path);
    if legacy_path.exists() {
        fs::remove_file(&legacy_path)
            .with_context(|| format!("failed to remove {}", legacy_path.display()))?;
    }
    Ok(())
}

fn merge_missing_values(target: &mut Vec<String>, defaults: Vec<String>) -> bool {
    let mut changed = false;
    for value in defaults {
        if !target.iter().any(|existing| existing == &value) {
            target.push(value);
            changed = true;
        }
    }
    changed
}

fn normalize_toml(content: &str) -> String {
    content
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}
