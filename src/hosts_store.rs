use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
#[cfg(windows)]
use std::{thread, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

#[cfg(windows)]
use std::ffi::OsStr;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(windows)]
use std::os::windows::fs::MetadataExt;
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY, REPLACEFILE_WRITE_THROUGH, ReplaceFileW,
    SetFileAttributesW,
};

use crate::paths::AppPaths;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HostsBackupMeta {
    source_path: String,
    backup_created_at_unix_ms: u64,
    original_attributes: HostsFileAttributes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HostsFileAttributes {
    readonly: bool,
    #[cfg(windows)]
    raw_file_attributes: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackupState {
    Missing,
    Ready,
    Inconsistent,
}

pub(crate) fn ensure_hosts_backup(
    paths: &AppPaths,
    source_path: &Path,
    backup_content: &str,
) -> Result<()> {
    match backup_state(paths) {
        BackupState::Ready => {
            validate_hosts_backup(paths, source_path)?;
            return Ok(());
        }
        BackupState::Inconsistent => {
            bail!(
                "hosts backup state is inconsistent; expected both {} and {}",
                paths.hosts_backup_path.display(),
                paths.hosts_backup_meta_path.display()
            );
        }
        BackupState::Missing => {}
    }

    let attributes = snapshot_file_attributes(source_path)?;
    let meta = HostsBackupMeta {
        source_path: source_path.display().to_string(),
        backup_created_at_unix_ms: unix_timestamp_ms()?,
        original_attributes: attributes,
    };
    let backup_temp_path = temp_backup_path(&paths.hosts_backup_path);
    let meta_temp_path = temp_backup_path(&paths.hosts_backup_meta_path);

    let serialized =
        serde_json::to_vec_pretty(&meta).context("failed to serialize hosts backup metadata")?;

    cleanup_temp_file(&backup_temp_path)?;
    cleanup_temp_file(&meta_temp_path)?;
    write_temp_file(&backup_temp_path, backup_content)?;
    write_temp_file(
        &meta_temp_path,
        std::str::from_utf8(&serialized).context("failed to encode hosts backup metadata")?,
    )?;

    if let Err(error) = fs::rename(&backup_temp_path, &paths.hosts_backup_path)
        .with_context(|| format!("failed to move {}", backup_temp_path.display()))
    {
        let _ = cleanup_temp_file(&backup_temp_path);
        let _ = cleanup_temp_file(&meta_temp_path);
        return Err(error);
    }

    if let Err(error) = fs::rename(&meta_temp_path, &paths.hosts_backup_meta_path)
        .with_context(|| format!("failed to move {}", meta_temp_path.display()))
    {
        let _ = fs::remove_file(&paths.hosts_backup_path);
        let _ = cleanup_temp_file(&backup_temp_path);
        let _ = cleanup_temp_file(&meta_temp_path);
        return Err(error);
    }

    Ok(())
}

pub(crate) fn write_hosts_content(
    path: &Path,
    original_content: &str,
    new_content: &str,
) -> Result<()> {
    let current_attributes = snapshot_file_attributes(path)?;
    write_hosts_content_inner(
        path,
        original_content,
        new_content,
        &current_attributes,
        &current_attributes,
    )
}

pub(crate) fn restore_hosts_from_backup(
    paths: &AppPaths,
    source_path: &Path,
    current_content: &str,
) -> Result<()> {
    validate_hosts_backup(paths, source_path)?;

    let backup = fs::read_to_string(&paths.hosts_backup_path)
        .with_context(|| format!("failed to read {}", paths.hosts_backup_path.display()))?;
    let meta = load_backup_meta(paths)?;
    if !source_path_matches(source_path, &meta.source_path) {
        bail!(
            "hosts backup source path mismatch; expected {}, got {}",
            source_path.display(),
            meta.source_path
        );
    }

    if backup == current_content {
        restore_file_attributes(source_path, &meta.original_attributes).with_context(|| {
            format!(
                "failed to restore hosts attributes {}",
                source_path.display()
            )
        })?;
        return Ok(());
    }

    let current_attributes = snapshot_file_attributes(source_path)?;
    write_hosts_content_inner(
        source_path,
        current_content,
        &backup,
        &current_attributes,
        &meta.original_attributes,
    )
    .with_context(|| format!("failed to restore hosts file {}", source_path.display()))
}

pub(crate) fn backup_state(paths: &AppPaths) -> BackupState {
    match (
        paths.hosts_backup_path.exists(),
        paths.hosts_backup_meta_path.exists(),
    ) {
        (false, false) => BackupState::Missing,
        (true, true) => BackupState::Ready,
        _ => BackupState::Inconsistent,
    }
}

pub(crate) fn validate_hosts_backup(paths: &AppPaths, source_path: &Path) -> Result<()> {
    match backup_state(paths) {
        BackupState::Ready => {}
        BackupState::Missing => {
            bail!(
                "hosts backup is unavailable; expected both {} and {}",
                paths.hosts_backup_path.display(),
                paths.hosts_backup_meta_path.display()
            );
        }
        BackupState::Inconsistent => {
            bail!(
                "hosts backup state is inconsistent; expected both {} and {}",
                paths.hosts_backup_path.display(),
                paths.hosts_backup_meta_path.display()
            );
        }
    }

    let _ = fs::read_to_string(&paths.hosts_backup_path)
        .with_context(|| format!("failed to read {}", paths.hosts_backup_path.display()))?;
    let meta = load_backup_meta(paths)?;
    if !source_path_matches(source_path, &meta.source_path) {
        bail!(
            "hosts backup source path mismatch; expected {}, got {}",
            source_path.display(),
            meta.source_path
        );
    }
    Ok(())
}

pub(crate) fn clear_hosts_backup(paths: &AppPaths) -> Result<()> {
    let backup_temp_path = temp_backup_path(&paths.hosts_backup_path);
    let meta_temp_path = temp_backup_path(&paths.hosts_backup_meta_path);
    let mut errors = Vec::new();

    for path in [
        &paths.hosts_backup_path,
        &paths.hosts_backup_meta_path,
        &backup_temp_path,
        &meta_temp_path,
    ] {
        if let Err(error) = cleanup_temp_file(path) {
            errors.push(format!("failed to remove {}: {error:#}", path.display()));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        bail!(errors.join("; "));
    }
}

fn load_backup_meta(paths: &AppPaths) -> Result<HostsBackupMeta> {
    let raw = fs::read(&paths.hosts_backup_meta_path)
        .with_context(|| format!("failed to read {}", paths.hosts_backup_meta_path.display()))?;
    serde_json::from_slice(&raw).context("failed to parse hosts backup metadata")
}

fn source_path_matches(source_path: &Path, recorded_path: &str) -> bool {
    #[cfg(windows)]
    {
        return source_path
            .to_string_lossy()
            .replace('/', "\\")
            .eq_ignore_ascii_case(&recorded_path.replace('/', "\\"));
    }

    #[cfg(not(windows))]
    {
        source_path.to_string_lossy() == recorded_path
    }
}

fn write_hosts_content_inner(
    path: &Path,
    original_content: &str,
    new_content: &str,
    current_attributes: &HostsFileAttributes,
    desired_attributes: &HostsFileAttributes,
) -> Result<()> {
    let temp_path = temp_hosts_path(path);
    let mut replaced = false;

    if temp_path.exists() {
        let _ = fs::remove_file(&temp_path);
    }

    if current_attributes.readonly {
        set_file_writable(path, current_attributes)?;
    }

    let result = (|| -> Result<()> {
        write_temp_file(&temp_path, new_content)?;
        if let Err(error) = replace_file_atomically(&temp_path, path) {
            write_hosts_content_without_replace(path, new_content, desired_attributes)
                .with_context(|| {
                    format!(
                        "failed to update hosts file {} after atomic replace fallback: {error:#}",
                        path.display()
                    )
                })?;
            replaced = true;
            let _ = cleanup_temp_file(&temp_path);
            return Ok(());
        }
        replaced = true;
        restore_file_attributes(path, desired_attributes)?;
        Ok(())
    })();

    if let Err(error) = result {
        let _ = cleanup_temp_file(&temp_path);

        if !replaced && path.exists() {
            let _ = restore_file_attributes(path, current_attributes);
        } else if replaced && path.exists() {
            let _ = write_hosts_content_without_replace(path, original_content, current_attributes);
        }

        return Err(error);
    }

    Ok(())
}

fn write_hosts_content_without_replace(
    path: &Path,
    content: &str,
    final_attributes: &HostsFileAttributes,
) -> Result<()> {
    if final_attributes.readonly {
        set_file_writable(path, final_attributes)?;
    }
    fs::write(path, content).with_context(|| format!("failed to rewrite {}", path.display()))?;
    restore_file_attributes(path, final_attributes)?;
    Ok(())
}

fn write_temp_file(path: &Path, content: &str) -> Result<()> {
    let mut file =
        File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
    file.write_all(content.as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync {}", path.display()))?;
    Ok(())
}

fn cleanup_temp_file(path: &Path) -> Result<()> {
    if path.exists() {
        fs::remove_file(path).with_context(|| format!("failed to remove {}", path.display()))?;
    }
    Ok(())
}

fn temp_hosts_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("hosts");
    path.with_file_name(format!("{file_name}.linuxdo-accelerator.tmp"))
}

fn temp_backup_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("backup");
    path.with_file_name(format!("{file_name}.linuxdo-accelerator.tmp"))
}

fn unix_timestamp_ms() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| anyhow!("system time is before unix epoch: {error}"))?
        .as_millis() as u64)
}

fn snapshot_file_attributes(path: &Path) -> Result<HostsFileAttributes> {
    let metadata =
        fs::metadata(path).with_context(|| format!("failed to stat {}", path.display()))?;

    #[cfg(windows)]
    {
        Ok(HostsFileAttributes {
            readonly: metadata.permissions().readonly(),
            raw_file_attributes: metadata.file_attributes(),
        })
    }

    #[cfg(not(windows))]
    {
        Ok(HostsFileAttributes {
            readonly: metadata.permissions().readonly(),
        })
    }
}

fn set_file_writable(path: &Path, attributes: &HostsFileAttributes) -> Result<()> {
    #[cfg(windows)]
    {
        let writable_attributes = normalize_windows_file_attributes(
            attributes.raw_file_attributes & !FILE_ATTRIBUTE_READONLY,
        );
        set_windows_file_attributes(path, writable_attributes)
    }

    #[cfg(not(windows))]
    {
        let _ = attributes;
        let mut permissions = fs::metadata(path)
            .with_context(|| format!("failed to stat {}", path.display()))?
            .permissions();
        permissions.set_readonly(false);
        fs::set_permissions(path, permissions)
            .with_context(|| format!("failed to clear readonly on {}", path.display()))
    }
}

fn restore_file_attributes(path: &Path, attributes: &HostsFileAttributes) -> Result<()> {
    #[cfg(windows)]
    {
        set_windows_file_attributes(
            path,
            normalize_windows_file_attributes(attributes.raw_file_attributes),
        )
    }

    #[cfg(not(windows))]
    {
        let mut permissions = fs::metadata(path)
            .with_context(|| format!("failed to stat {}", path.display()))?
            .permissions();
        permissions.set_readonly(attributes.readonly);
        fs::set_permissions(path, permissions)
            .with_context(|| format!("failed to set permissions on {}", path.display()))
    }
}

#[cfg(windows)]
fn set_windows_file_attributes(path: &Path, attributes: u32) -> Result<()> {
    let wide = wide_null(path.as_os_str());
    let result = unsafe { SetFileAttributesW(wide.as_ptr(), attributes) };
    if result == 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to set file attributes {}", path.display()));
    }
    Ok(())
}

#[cfg(windows)]
fn normalize_windows_file_attributes(attributes: u32) -> u32 {
    if attributes == 0 {
        FILE_ATTRIBUTE_NORMAL
    } else {
        attributes
    }
}

#[cfg(windows)]
fn replace_file_atomically(temp_path: &Path, target_path: &Path) -> Result<()> {
    let temp_wide = wide_null(temp_path.as_os_str());
    let target_wide = wide_null(target_path.as_os_str());
    const MAX_RETRIES: u32 = 8;

    for attempt in 0..MAX_RETRIES {
        let replaced = unsafe {
            ReplaceFileW(
                target_wide.as_ptr(),
                temp_wide.as_ptr(),
                std::ptr::null(),
                REPLACEFILE_WRITE_THROUGH,
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        if replaced != 0 {
            return Ok(());
        }

        let error = std::io::Error::last_os_error();
        let is_last_attempt = attempt + 1 >= MAX_RETRIES;
        if is_last_attempt || !should_retry_windows_replace_error(&error) {
            return Err(error).with_context(|| {
                format!(
                    "failed to replace {} with {}",
                    target_path.display(),
                    temp_path.display()
                )
            });
        }

        thread::sleep(replace_retry_delay(attempt));
    }

    unreachable!("windows replace retry loop should have returned before reaching the end")
}

#[cfg(windows)]
fn should_retry_windows_replace_error(error: &std::io::Error) -> bool {
    should_retry_windows_replace_error_code(error.raw_os_error())
}

#[cfg(windows)]
fn should_retry_windows_replace_error_code(code: Option<i32>) -> bool {
    matches!(code, Some(5 | 32 | 33 | 1175))
}

#[cfg(windows)]
fn replace_retry_delay(attempt: u32) -> Duration {
    Duration::from_millis(80 * u64::from(attempt + 1))
}

#[cfg(not(windows))]
fn replace_file_atomically(temp_path: &Path, target_path: &Path) -> Result<()> {
    fs::rename(temp_path, target_path).with_context(|| {
        format!(
            "failed to replace {} with {}",
            target_path.display(),
            temp_path.display()
        )
    })
}

#[cfg(windows)]
fn wide_null(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{
        BackupState, backup_state, clear_hosts_backup, ensure_hosts_backup, load_backup_meta,
        restore_hosts_from_backup, validate_hosts_backup, write_hosts_content,
    };
    use crate::paths::AppPaths;

    static NEXT_ID: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn write_hosts_content_restores_readonly_attribute() {
        let test_dir = create_test_dir("write_hosts_content_restores_readonly_attribute");
        let file_path = test_dir.join("hosts");
        std::fs::write(&file_path, "before").unwrap();

        let mut permissions = std::fs::metadata(&file_path).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&file_path, permissions).unwrap();

        write_hosts_content(&file_path, "before", "after").unwrap();

        assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "after");
        assert!(
            std::fs::metadata(&file_path)
                .unwrap()
                .permissions()
                .readonly()
        );

        cleanup_test_dir(&test_dir);
    }

    #[test]
    fn ensure_hosts_backup_only_keeps_first_snapshot() {
        let test_dir = create_test_dir("ensure_hosts_backup_only_keeps_first_snapshot");
        let paths = test_paths(&test_dir);
        std::fs::create_dir_all(&paths.runtime_dir).unwrap();
        let source_path = test_dir.join("hosts");

        std::fs::write(&source_path, "first").unwrap();
        ensure_hosts_backup(&paths, &source_path, "first").unwrap();

        std::fs::write(&source_path, "second").unwrap();
        ensure_hosts_backup(&paths, &source_path, "second").unwrap();

        assert_eq!(
            std::fs::read_to_string(&paths.hosts_backup_path).unwrap(),
            "first"
        );

        cleanup_test_dir(&test_dir);
    }

    #[test]
    fn restore_hosts_from_backup_rejects_inconsistent_backup_state() {
        let test_dir =
            create_test_dir("restore_hosts_from_backup_rejects_inconsistent_backup_state");
        let paths = test_paths(&test_dir);
        std::fs::create_dir_all(&paths.runtime_dir).unwrap();
        let source_path = test_dir.join("hosts");
        std::fs::write(&source_path, "current").unwrap();
        std::fs::write(&paths.hosts_backup_path, "backup").unwrap();

        let error = restore_hosts_from_backup(&paths, &source_path, "current").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("hosts backup state is inconsistent")
        );

        cleanup_test_dir(&test_dir);
    }

    #[test]
    fn restore_hosts_from_backup_rejects_source_path_mismatch() {
        let test_dir = create_test_dir("restore_hosts_from_backup_rejects_source_path_mismatch");
        let paths = test_paths(&test_dir);
        std::fs::create_dir_all(&paths.runtime_dir).unwrap();
        let source_path = test_dir.join("hosts");
        std::fs::write(&source_path, "original").unwrap();
        ensure_hosts_backup(&paths, &source_path, "original").unwrap();

        let mut meta = load_backup_meta(&paths).unwrap();
        meta.source_path = r"C:\different\hosts".to_string();
        std::fs::write(
            &paths.hosts_backup_meta_path,
            serde_json::to_string_pretty(&meta).unwrap(),
        )
        .unwrap();

        let error = restore_hosts_from_backup(&paths, &source_path, "original").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("hosts backup source path mismatch")
        );

        cleanup_test_dir(&test_dir);
    }

    #[test]
    fn validate_hosts_backup_rejects_broken_metadata() {
        let test_dir = create_test_dir("validate_hosts_backup_rejects_broken_metadata");
        let paths = test_paths(&test_dir);
        std::fs::create_dir_all(&paths.runtime_dir).unwrap();
        let source_path = test_dir.join("hosts");

        std::fs::write(&source_path, "original").unwrap();
        std::fs::write(&paths.hosts_backup_path, "backup").unwrap();
        std::fs::write(&paths.hosts_backup_meta_path, "{not-json").unwrap();

        let error = validate_hosts_backup(&paths, &source_path).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("failed to parse hosts backup metadata")
        );

        cleanup_test_dir(&test_dir);
    }

    #[test]
    fn clear_hosts_backup_removes_partial_and_temp_files() {
        let test_dir = create_test_dir("clear_hosts_backup_removes_partial_and_temp_files");
        let paths = test_paths(&test_dir);
        std::fs::create_dir_all(&paths.runtime_dir).unwrap();

        std::fs::write(&paths.hosts_backup_path, "backup").unwrap();
        let backup_temp_path = paths
            .hosts_backup_path
            .with_file_name("hosts.backup.linuxdo-accelerator.tmp");
        let meta_temp_path = paths
            .hosts_backup_meta_path
            .with_file_name("hosts.backup.json.linuxdo-accelerator.tmp");
        std::fs::write(&backup_temp_path, "temp").unwrap();
        std::fs::write(&meta_temp_path, "temp").unwrap();

        clear_hosts_backup(&paths).unwrap();

        assert_eq!(backup_state(&paths), BackupState::Missing);
        assert!(!backup_temp_path.exists());
        assert!(!meta_temp_path.exists());

        cleanup_test_dir(&test_dir);
    }

    #[cfg(windows)]
    #[test]
    fn windows_retryable_replace_errors_cover_common_lock_cases() {
        assert!(super::should_retry_windows_replace_error_code(Some(5)));
        assert!(super::should_retry_windows_replace_error_code(Some(32)));
        assert!(super::should_retry_windows_replace_error_code(Some(33)));
        assert!(!super::should_retry_windows_replace_error_code(Some(2)));
        assert!(!super::should_retry_windows_replace_error_code(None));
    }

    fn create_test_dir(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        path.push(format!("linuxdo-accelerator-{name}-{id}"));
        if path.exists() {
            let _ = std::fs::remove_dir_all(&path);
        }
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn cleanup_test_dir(path: &PathBuf) {
        if path.exists() {
            clear_readonly_recursively(path);
        }
        let _ = std::fs::remove_dir_all(path);
    }

    fn clear_readonly_recursively(path: &PathBuf) {
        if path.is_dir() {
            if let Ok(entries) = std::fs::read_dir(path) {
                for entry in entries.flatten() {
                    let child = entry.path();
                    let child_path = PathBuf::from(child);
                    clear_readonly_recursively(&child_path);
                }
            }
        }

        if let Ok(metadata) = std::fs::metadata(path) {
            let mut permissions = metadata.permissions();
            if permissions.readonly() {
                permissions.set_readonly(false);
                let _ = std::fs::set_permissions(path, permissions);
            }
        }
    }

    fn test_paths(root: &PathBuf) -> AppPaths {
        let config_dir = root.join("config");
        let data_dir = root.join("data");
        let runtime_dir = data_dir.join("runtime");
        let cert_dir = data_dir.join("certs");

        AppPaths {
            config_path: config_dir.join("linuxdo-accelerator.toml"),
            config_dir,
            data_dir,
            runtime_dir: runtime_dir.clone(),
            cert_dir,
            state_path: runtime_dir.join("service-state.json"),
            pid_path: runtime_dir.join("linuxdo-accelerator.pid"),
            runtime_log_path: runtime_dir.join("operations.log"),
            hosts_backup_path: runtime_dir.join("hosts.backup"),
            hosts_backup_meta_path: runtime_dir.join("hosts.backup.json"),
        }
    }
}
