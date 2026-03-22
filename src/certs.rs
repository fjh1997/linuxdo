use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyUsagePurpose,
};
use time::{Duration, OffsetDateTime};

use crate::config::AppConfig;
use crate::platform::sync_user_ownership;

#[derive(Debug, Clone)]
pub struct CertificateBundle {
    pub ca_cert_path: PathBuf,
    pub server_cert_path: PathBuf,
    pub server_key_path: PathBuf,
}

const CA_VALIDITY_DAYS: i64 = 365;
const SERVER_VALIDITY_DAYS: i64 = 365;

pub fn ensure_bundle(config: &AppConfig, root: &Path) -> Result<CertificateBundle> {
    generate_bundle(config, root, true)
}

pub fn load_or_create_bundle(config: &AppConfig, root: &Path) -> Result<CertificateBundle> {
    generate_bundle(config, root, false)
}

fn generate_bundle(
    config: &AppConfig,
    root: &Path,
    force_regenerate: bool,
) -> Result<CertificateBundle> {
    let cert_dir = root.to_path_buf();
    fs::create_dir_all(&cert_dir)
        .with_context(|| format!("failed to create {}", cert_dir.display()))?;

    let bundle = CertificateBundle {
        ca_cert_path: cert_dir.join("linuxdo-accelerator-root-ca.crt"),
        server_cert_path: cert_dir.join("linuxdo-accelerator-server.crt"),
        server_key_path: cert_dir.join("linuxdo-accelerator-server.key"),
    };

    if !force_regenerate
        && bundle.ca_cert_path.exists()
        && bundle.server_cert_path.exists()
        && bundle.server_key_path.exists()
    {
        return Ok(bundle);
    }

    remove_existing_bundle(&bundle)?;

    let (ca_not_before, ca_not_after) = validity_window(CA_VALIDITY_DAYS)?;
    let mut ca_params = CertificateParams::default();
    ca_params.alg = &rcgen::PKCS_ECDSA_P256_SHA256;
    ca_params.not_before = ca_not_before;
    ca_params.not_after = ca_not_after;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.distinguished_name = DistinguishedName::new();
    ca_params
        .distinguished_name
        .push(DnType::CommonName, config.ca_common_name.clone());
    ca_params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
    ];

    let ca_cert = Certificate::from_params(ca_params).context("failed to generate root CA")?;

    let (server_not_before, server_not_after) = validity_window(SERVER_VALIDITY_DAYS)?;
    let mut server_params = CertificateParams::new(config.certificate_domains.clone());
    server_params.alg = &rcgen::PKCS_ECDSA_P256_SHA256;
    server_params.not_before = server_not_before;
    server_params.not_after = server_not_after;
    server_params.distinguished_name = DistinguishedName::new();
    server_params
        .distinguished_name
        .push(DnType::CommonName, config.server_common_name.clone());
    server_params.is_ca = IsCa::ExplicitNoCa;
    server_params.use_authority_key_identifier_extension = true;
    server_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];

    let server_cert =
        Certificate::from_params(server_params).context("failed to generate server cert")?;

    fs::write(
        &bundle.ca_cert_path,
        ca_cert
            .serialize_pem()
            .context("failed to serialize root CA")?,
    )
    .with_context(|| format!("failed to write {}", bundle.ca_cert_path.display()))?;
    fs::write(
        &bundle.server_cert_path,
        server_cert
            .serialize_pem_with_signer(&ca_cert)
            .context("failed to sign server cert")?,
    )
    .with_context(|| format!("failed to write {}", bundle.server_cert_path.display()))?;
    fs::write(
        &bundle.server_key_path,
        server_cert.serialize_private_key_pem(),
    )
    .with_context(|| format!("failed to write {}", bundle.server_key_path.display()))?;
    sync_user_ownership(&bundle.ca_cert_path)?;
    sync_user_ownership(&bundle.server_cert_path)?;
    sync_user_ownership(&bundle.server_key_path)?;

    Ok(bundle)
}

fn remove_existing_bundle(bundle: &CertificateBundle) -> Result<()> {
    let legacy_version_path = bundle
        .ca_cert_path
        .parent()
        .unwrap_or(Path::new(""))
        .join("schema-version");
    for path in [
        &bundle.ca_cert_path,
        &bundle.server_cert_path,
        &bundle.server_key_path,
        &legacy_version_path,
    ] {
        match fs::remove_file(path) {
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("failed to remove {}", path.display()));
            }
        }
    }

    Ok(())
}

fn validity_window(valid_days: i64) -> Result<(OffsetDateTime, OffsetDateTime)> {
    let now = OffsetDateTime::now_utc();
    let not_before = now
        .checked_sub(Duration::days(1))
        .ok_or_else(|| anyhow!("failed to compute certificate not_before"))?;
    let not_after = now
        .checked_add(Duration::days(valid_days))
        .ok_or_else(|| anyhow!("failed to compute certificate not_after"))?;
    Ok((not_before, not_after))
}
