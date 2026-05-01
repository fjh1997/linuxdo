use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

const DEFAULT_APP_CONFIG: &str = include_str!("../assets/defaults/linuxdo-accelerator.toml");
const CURRENT_CONFIG_VERSION: &str = env!("LINUXDO_GIT_HASH");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_listen_host")]
    pub listen_host: String,
    #[serde(default = "default_hosts_ip")]
    pub hosts_ip: String,
    #[serde(default = "default_http_port")]
    pub http_port: u16,
    #[serde(default = "default_https_port")]
    pub https_port: u16,
    #[serde(default = "default_upstream")]
    pub upstream: String,
    #[serde(default = "default_fake_sni")]
    pub fake_sni: Option<String>,
    #[serde(default = "default_doh_endpoints")]
    pub doh_endpoints: Vec<String>,
    #[serde(default = "default_managed_prefer_ipv6")]
    pub managed_prefer_ipv6: bool,
    #[serde(default)]
    pub dns_hosts: BTreeMap<String, String>,
    #[serde(default)]
    pub edge_node: Option<String>,
    #[serde(default = "default_proxy_domains")]
    pub proxy_domains: Vec<String>,
    #[serde(default)]
    pub hosts_domains: Vec<String>,
    #[serde(default = "default_certificate_domains")]
    pub certificate_domains: Vec<String>,
    #[serde(default = "default_ca_common_name")]
    pub ca_common_name: String,
    #[serde(default = "default_server_common_name")]
    pub server_common_name: String,
    #[serde(default)]
    pub autostart: bool,
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

fn default_listen_host() -> String {
    default_app_config().listen_host
}

fn default_hosts_ip() -> String {
    default_app_config().hosts_ip
}

fn default_http_port() -> u16 {
    default_app_config().http_port
}

fn default_https_port() -> u16 {
    default_app_config().https_port
}

fn default_upstream() -> String {
    default_app_config().upstream
}

fn default_doh_endpoints() -> Vec<String> {
    default_app_config().doh_endpoints
}

fn default_managed_prefer_ipv6() -> bool {
    false
}

fn default_proxy_domains() -> Vec<String> {
    default_app_config().proxy_domains
}

fn default_certificate_domains() -> Vec<String> {
    default_app_config().certificate_domains
}

fn default_ca_common_name() -> String {
    default_app_config().ca_common_name
}

fn default_server_common_name() -> String {
    default_app_config().server_common_name
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

fn sibling_path(config_path: &Path, suffix: &str) -> PathBuf {
    let mut name = config_path
        .file_name()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| OsString::from("linuxdo-accelerator.toml"));
    name.push(suffix);
    config_path
        .parent()
        .map(|parent| parent.join(&name))
        .unwrap_or_else(|| PathBuf::from(&name))
}

fn version_marker_path(config_path: &Path) -> PathBuf {
    sibling_path(config_path, ".version")
}

fn backup_config_path(config_path: &Path) -> PathBuf {
    sibling_path(config_path, ".bak")
}

fn read_version_marker(marker_path: &Path) -> Option<String> {
    fs::read_to_string(marker_path)
        .ok()
        .map(|value| value.trim().to_string())
}

fn write_version_marker(marker_path: &Path) -> Result<()> {
    fs::write(marker_path, CURRENT_CONFIG_VERSION)
        .with_context(|| format!("failed to write {}", marker_path.display()))
}

fn marker_matches_current_version(marker_path: &Path) -> bool {
    matches!(read_version_marker(marker_path), Some(value) if value == CURRENT_CONFIG_VERSION)
}

impl AppConfig {
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        let marker_path = version_marker_path(path);

        if path.exists() && !marker_matches_current_version(&marker_path) {
            let backup_path = backup_config_path(path);
            fs::copy(path, &backup_path).with_context(|| {
                format!(
                    "failed to back up {} to {}",
                    path.display(),
                    backup_path.display()
                )
            })?;
            fs::write(path, DEFAULT_APP_CONFIG)
                .with_context(|| format!("failed to write config {}", path.display()))?;
            write_version_marker(&marker_path)?;
            cleanup_legacy_network_profile(path)?;
            return Ok(default_app_config());
        }

        if !path.exists() {
            fs::write(path, DEFAULT_APP_CONFIG)
                .with_context(|| format!("failed to write config {}", path.display()))?;
            write_version_marker(&marker_path)?;
            cleanup_legacy_network_profile(path)?;
            return Ok(default_app_config());
        }

        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;

        if let Ok(config) = toml::from_str::<AppConfig>(&content) {
            cleanup_legacy_network_profile(path)?;
            return Ok(config);
        }

        let legacy: LegacyMainConfig = toml::from_str(&content)
            .with_context(|| format!("failed to parse config {}", path.display()))?;
        let legacy_network = load_legacy_network_profile(path)?;
        let defaults = default_app_config();
        let config = AppConfig {
            listen_host: legacy.listen_host,
            hosts_ip: legacy.hosts_ip,
            http_port: legacy.http_port,
            https_port: legacy.https_port,
            upstream: legacy.upstream,
            fake_sni: legacy.fake_sni,
            doh_endpoints: legacy_network
                .as_ref()
                .map(|value| value.doh_endpoints.clone())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(default_doh_endpoints),
            managed_prefer_ipv6: default_managed_prefer_ipv6(),
            dns_hosts: BTreeMap::new(),
            edge_node: None,
            proxy_domains: legacy_network
                .as_ref()
                .map(|value| value.proxy_domains.clone())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(default_proxy_domains),
            hosts_domains: defaults.hosts_domains,
            certificate_domains: legacy_network
                .as_ref()
                .map(|value| value.certificate_domains.clone())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(default_certificate_domains),
            ca_common_name: legacy.ca_common_name,
            server_common_name: legacy.server_common_name,
            autostart: false,
        };

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
            write_version_marker(&version_marker_path(path))?;
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

    pub fn edge_node_override(&self) -> Option<&str> {
        self.edge_node
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
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

#[cfg(test)]
mod tests {
    use super::{
        AppConfig, CURRENT_CONFIG_VERSION, DEFAULT_APP_CONFIG, backup_config_path,
        version_marker_path,
    };
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ID: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn load_or_create_preserves_manual_loopback_override() {
        let root = create_test_dir("preserve-manual-loopback");
        let config_path = root.join("linuxdo-accelerator.toml");
        let customized = DEFAULT_APP_CONFIG.replace("127.211.73.84", "127.0.0.1");
        std::fs::write(&config_path, customized).unwrap();
        write_current_version_marker(&config_path);

        let config = AppConfig::load_or_create(&config_path).unwrap();

        assert_eq!(config.listen_host, "127.0.0.1");
        assert_eq!(config.hosts_ip, "127.0.0.1");
        assert!(!backup_config_path(&config_path).exists());

        cleanup_test_dir(&root);
    }

    #[test]
    fn load_or_create_does_not_restore_removed_default_entries() {
        let root = create_test_dir("preserve-custom-lists");
        let config_path = root.join("linuxdo-accelerator.toml");
        let customized = r#"
listen_host = "127.0.0.1"
hosts_ip = "127.0.0.1"
http_port = 8080
https_port = 8443
upstream = "https://linux.do"
fake_sni = "www.cloudflare.com"
doh_endpoints = ["https://1.1.1.1/dns-query"]
managed_prefer_ipv6 = false
dns_hosts = {}
proxy_domains = ["linux.do"]
hosts_domains = ["linux.do"]
certificate_domains = ["linux.do"]
ca_common_name = "Linux.do Accelerator Root CA"
server_common_name = "linux.do"
"#;
        std::fs::write(&config_path, customized).unwrap();
        write_current_version_marker(&config_path);

        let config = AppConfig::load_or_create(&config_path).unwrap();
        let reloaded = std::fs::read_to_string(&config_path).unwrap();

        assert_eq!(config.proxy_domains, vec!["linux.do"]);
        assert_eq!(config.hosts_domains, vec!["linux.do"]);
        assert_eq!(config.certificate_domains, vec!["linux.do"]);
        assert_eq!(config.doh_endpoints, vec!["https://1.1.1.1/dns-query"]);
        assert_eq!(reloaded, customized);
        assert!(!backup_config_path(&config_path).exists());

        cleanup_test_dir(&root);
    }

    #[test]
    fn load_or_create_uses_defaults_for_missing_fields_without_rewriting() {
        let root = create_test_dir("missing-fields");
        let config_path = root.join("linuxdo-accelerator.toml");
        let customized = r#"
listen_host = "127.0.0.1"
hosts_ip = "127.0.0.1"
upstream = "https://linux.do"
doh_endpoints = ["https://1.1.1.1/dns-query"]
proxy_domains = ["linux.do"]
hosts_domains = ["linux.do"]
certificate_domains = ["linux.do"]
ca_common_name = "Linux.do Accelerator Root CA"
server_common_name = "linux.do"
"#;
        std::fs::write(&config_path, customized).unwrap();
        write_current_version_marker(&config_path);

        let config = AppConfig::load_or_create(&config_path).unwrap();
        let reloaded = std::fs::read_to_string(&config_path).unwrap();

        assert_eq!(config.listen_host, "127.0.0.1");
        assert_eq!(config.hosts_ip, "127.0.0.1");
        assert_eq!(config.http_port, 80);
        assert_eq!(config.https_port, 443);
        assert_eq!(config.fake_sni, Some("www.cloudflare.com".to_string()));
        assert_eq!(reloaded, customized);

        cleanup_test_dir(&root);
    }

    #[test]
    fn load_or_create_resets_to_defaults_on_version_mismatch_and_keeps_backup() {
        let root = create_test_dir("upgrade-overwrite");
        let config_path = root.join("linuxdo-accelerator.toml");
        let legacy = r#"
listen_host = "127.0.0.1"
hosts_ip = "127.0.0.1"
upstream = "https://linux.do"
doh_endpoints = ["https://example.test/dns-query"]
proxy_domains = ["linux.do"]
hosts_domains = ["linux.do"]
certificate_domains = ["linux.do"]
ca_common_name = "Linux.do Accelerator Root CA"
server_common_name = "linux.do"
"#;
        std::fs::write(&config_path, legacy).unwrap();
        std::fs::write(version_marker_path(&config_path), "0.0.0-old").unwrap();

        let config = AppConfig::load_or_create(&config_path).unwrap();
        let on_disk = std::fs::read_to_string(&config_path).unwrap();
        let backup = std::fs::read_to_string(backup_config_path(&config_path)).unwrap();
        let marker = std::fs::read_to_string(version_marker_path(&config_path)).unwrap();

        assert_eq!(on_disk, DEFAULT_APP_CONFIG);
        assert_eq!(backup, legacy);
        assert_eq!(marker, CURRENT_CONFIG_VERSION);
        assert_eq!(config.listen_host, "127.211.73.84");

        cleanup_test_dir(&root);
    }

    #[test]
    fn load_or_create_resets_to_defaults_when_marker_is_missing() {
        let root = create_test_dir("upgrade-no-marker");
        let config_path = root.join("linuxdo-accelerator.toml");
        let legacy = "listen_host = \"127.0.0.1\"\n";
        std::fs::write(&config_path, legacy).unwrap();

        let _ = AppConfig::load_or_create(&config_path).unwrap();

        let on_disk = std::fs::read_to_string(&config_path).unwrap();
        let backup = std::fs::read_to_string(backup_config_path(&config_path)).unwrap();
        let marker = std::fs::read_to_string(version_marker_path(&config_path)).unwrap();
        assert_eq!(on_disk, DEFAULT_APP_CONFIG);
        assert_eq!(backup, legacy);
        assert_eq!(marker, CURRENT_CONFIG_VERSION);

        cleanup_test_dir(&root);
    }

    #[test]
    fn load_or_create_writes_marker_on_fresh_install() {
        let root = create_test_dir("fresh-install");
        let config_path = root.join("linuxdo-accelerator.toml");

        let _ = AppConfig::load_or_create(&config_path).unwrap();

        let marker = std::fs::read_to_string(version_marker_path(&config_path)).unwrap();
        assert_eq!(marker, CURRENT_CONFIG_VERSION);
        assert!(!backup_config_path(&config_path).exists());

        cleanup_test_dir(&root);
    }

    fn write_current_version_marker(config_path: &Path) {
        std::fs::write(version_marker_path(config_path), CURRENT_CONFIG_VERSION).unwrap();
    }

    fn create_test_dir(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        path.push(format!("linuxdo-accelerator-config-{name}-{id}"));
        if path.exists() {
            let _ = std::fs::remove_dir_all(&path);
        }
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn cleanup_test_dir(path: &std::path::Path) {
        if path.exists() {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}
