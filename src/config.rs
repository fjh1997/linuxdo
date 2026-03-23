use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

const DEFAULT_APP_CONFIG: &str = include_str!("../assets/defaults/linuxdo-accelerator.toml");
const OLD_DEFAULT_LOOPBACK: &str = "127.0.0.1";
const NEW_DEFAULT_LOOPBACK: &str = "127.211.73.84";

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
    #[serde(default)]
    pub dns_hosts: BTreeMap<String, String>,
    pub proxy_domains: Vec<String>,
    #[serde(default)]
    pub hosts_domains: Vec<String>,
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
            changed |= migrate_loopback_defaults(&mut config);
            changed |= merge_missing_values(&mut config.doh_endpoints, defaults.doh_endpoints);
            changed |= merge_missing_values(&mut config.proxy_domains, defaults.proxy_domains);
            changed |= merge_missing_values(&mut config.hosts_domains, defaults.hosts_domains);
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
            dns_hosts: BTreeMap::new(),
            proxy_domains: legacy_network
                .as_ref()
                .map(|value| value.proxy_domains.clone())
                .unwrap_or_default(),
            hosts_domains: Vec::new(),
            certificate_domains: legacy_network
                .as_ref()
                .map(|value| value.certificate_domains.clone())
                .unwrap_or_default(),
            ca_common_name: legacy.ca_common_name,
            server_common_name: legacy.server_common_name,
        };
        migrate_loopback_defaults(&mut config);
        merge_missing_values(&mut config.doh_endpoints, defaults.doh_endpoints);
        merge_missing_values(&mut config.proxy_domains, defaults.proxy_domains);
        merge_missing_values(&mut config.hosts_domains, defaults.hosts_domains);
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

    pub fn find_dns_host_override(&self, host: &str) -> Option<&str> {
        let candidate = host.to_ascii_lowercase();

        if let Some(target) = self.dns_hosts.get(&candidate) {
            return Some(target.as_str());
        }

        self.dns_hosts
            .iter()
            .filter_map(|(pattern, target)| {
                let pattern = pattern.to_ascii_lowercase();
                let suffix = pattern.strip_prefix("*.")?;
                candidate
                    .ends_with(&format!(".{suffix}"))
                    .then_some((suffix.len(), target.as_str()))
            })
            .max_by_key(|(suffix_len, _)| *suffix_len)
            .map(|(_, target)| target)
    }

    pub fn hosts_domains(&self) -> &[String] {
        if self.hosts_domains.is_empty() {
            &self.proxy_domains
        } else {
            &self.hosts_domains
        }
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

fn migrate_loopback_defaults(config: &mut AppConfig) -> bool {
    let mut changed = false;

    if config.listen_host == OLD_DEFAULT_LOOPBACK {
        config.listen_host = NEW_DEFAULT_LOOPBACK.to_string();
        changed = true;
    }

    if config.hosts_ip == OLD_DEFAULT_LOOPBACK {
        config.hosts_ip = NEW_DEFAULT_LOOPBACK.to_string();
        changed = true;
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
