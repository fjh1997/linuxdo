use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};

use crate::config::AppConfig;

const LINUX_CA_FILE_NAME: &str = "linuxdo-accelerator-root-ca.crt";

pub fn ensure_elevated(config: &AppConfig, for_setup: bool) -> Result<()> {
    let needs_privilege = for_setup || config.http_port < 1024 || config.https_port < 1024;
    if needs_privilege && !is_elevated() {
        bail!("this command requires administrator/root privileges");
    }
    Ok(())
}

pub fn is_elevated() -> bool {
    if cfg!(target_os = "windows") {
        return Command::new("fltmc")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false);
    }

    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim() == "0")
        .unwrap_or(false)
}

pub fn run_elevated(executable: &Path, args: &[String]) -> Result<()> {
    match std::env::consts::OS {
        "windows" => run_windows_elevated(executable, args),
        "macos" => run_macos_elevated(executable, args),
        _ => run_linux_elevated(executable, args),
    }
}

pub fn spawn_detached(executable: &Path, args: &[String]) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let mut command = Command::new("setsid");
        command
            .arg(executable)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        command
            .spawn()
            .with_context(|| format!("failed to spawn detached {}", executable.display()))?;
        return Ok(());
    }

    #[cfg(not(target_os = "linux"))]
    {
        let mut command = Command::new(executable);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        #[cfg(target_family = "unix")]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }

        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
        }

        command
            .spawn()
            .with_context(|| format!("failed to spawn detached {}", executable.display()))?;
        Ok(())
    }
}

pub fn terminate_process(pid: u32) -> Result<()> {
    if cfg!(target_os = "windows") {
        run_command("taskkill", &["/PID", &pid.to_string(), "/T", "/F"])?;
        return Ok(());
    }

    run_command("kill", &["-TERM", &pid.to_string()])?;
    Ok(())
}

pub fn is_process_running(pid: u32) -> bool {
    if cfg!(target_os = "windows") {
        return Command::new("cmd")
            .args(["/C", &format!("tasklist /FI \"PID eq {pid}\"")])
            .output()
            .ok()
            .map(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .any(|line| line.contains(&pid.to_string()))
            })
            .unwrap_or(false);
    }

    let kill_probe = Command::new("kill").args(["-0", &pid.to_string()]).output();

    match kill_probe {
        Ok(output) if output.status.success() => true,
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("Operation not permitted") {
                return true;
            }

            Command::new("ps")
                .args(["-p", &pid.to_string(), "-o", "pid="])
                .output()
                .ok()
                .map(|ps| {
                    ps.status.success() && !String::from_utf8_lossy(&ps.stdout).trim().is_empty()
                })
                .unwrap_or(false)
        }
        Err(_) => false,
    }
}

pub fn install_ca(ca_cert_path: &Path, common_name: &str) -> Result<()> {
    let _ = uninstall_ca(common_name);

    match std::env::consts::OS {
        "windows" => {
            run_command(
                "certutil",
                &["-f", "-addstore", "Root", &ca_cert_path.to_string_lossy()],
            )?;
        }
        "macos" => {
            run_command(
                "security",
                &[
                    "add-trusted-cert",
                    "-d",
                    "-r",
                    "trustRoot",
                    "-k",
                    "/Library/Keychains/System.keychain",
                    &ca_cert_path.to_string_lossy(),
                ],
            )?;
        }
        _ => {
            let dest = linux_trust_store_path()
                .ok_or_else(|| anyhow!("unsupported Linux trust store layout"))?;
            fs::create_dir_all(
                dest.parent()
                    .ok_or_else(|| anyhow!("invalid Linux trust store path"))?,
            )
            .with_context(|| format!("failed to create {}", dest.display()))?;
            fs::copy(ca_cert_path, &dest)
                .with_context(|| format!("failed to copy CA to {}", dest.display()))?;

            if dest.starts_with("/usr/local/share/ca-certificates") {
                run_command("update-ca-certificates", &[])?;
            } else {
                run_command("update-ca-trust", &["extract"])?;
            }

            #[cfg(target_os = "linux")]
            {
                install_ca_to_linux_nss(ca_cert_path, common_name)?;
                install_ca_to_firefox_profiles(ca_cert_path, common_name)?;
            }
        }
    }

    println!("installed root certificate: {common_name}");
    Ok(())
}

pub fn uninstall_ca(common_name: &str) -> Result<()> {
    match std::env::consts::OS {
        "windows" => {
            run_command("certutil", &["-delstore", "Root", common_name])?;
        }
        "macos" => {
            run_command(
                "security",
                &[
                    "delete-certificate",
                    "-c",
                    common_name,
                    "/Library/Keychains/System.keychain",
                ],
            )?;
        }
        _ => {
            if let Some(path) = linux_trust_store_path() {
                if path.exists() {
                    fs::remove_file(&path)
                        .with_context(|| format!("failed to remove {}", path.display()))?;
                }

                if path.starts_with("/usr/local/share/ca-certificates") {
                    run_command("update-ca-certificates", &[])?;
                } else {
                    run_command("update-ca-trust", &["extract"])?;
                }
            }

            #[cfg(target_os = "linux")]
            {
                uninstall_ca_from_linux_nss(common_name)?;
                uninstall_ca_from_firefox_profiles(common_name)?;
            }
        }
    }

    Ok(())
}

pub fn sync_user_ownership(_path: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        if !is_elevated() {
            return Ok(());
        }

        let Some((uid, gid)) = original_user_ids() else {
            return Ok(());
        };

        if _path.exists() {
            chown_recursive(_path, uid, gid)?;
        }
    }

    Ok(())
}

fn linux_trust_store_path() -> Option<PathBuf> {
    if Path::new("/usr/local/share/ca-certificates").exists() {
        return Some(PathBuf::from(format!(
            "/usr/local/share/ca-certificates/{LINUX_CA_FILE_NAME}"
        )));
    }
    if Path::new("/etc/pki/ca-trust/source/anchors").exists() {
        return Some(PathBuf::from(format!(
            "/etc/pki/ca-trust/source/anchors/{LINUX_CA_FILE_NAME}"
        )));
    }
    None
}

#[cfg(target_os = "linux")]
fn original_user_ids() -> Option<(u32, u32)> {
    let uid = std::env::var("PKEXEC_UID")
        .ok()
        .or_else(|| std::env::var("SUDO_UID").ok())
        .and_then(|value| value.parse::<u32>().ok())?;

    unsafe {
        let passwd = libc::getpwuid(uid);
        if passwd.is_null() {
            return None;
        }
        Some((uid, (*passwd).pw_gid))
    }
}

#[cfg(target_os = "linux")]
fn chown_recursive(path: &Path, uid: u32, gid: u32) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    chown_path(path, uid, gid)?;

    if path.is_dir() {
        for entry in fs::read_dir(path)
            .with_context(|| format!("failed to read directory {}", path.display()))?
        {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to walk directory {}", path.display()));
                }
            };
            chown_recursive(&entry.path(), uid, gid)?;
        }
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn chown_path(path: &Path, uid: u32, gid: u32) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("invalid path {}", path.display()))?;
    let result = unsafe { libc::chown(c_path.as_ptr(), uid, gid) };
    if result != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to chown {}", path.display()));
    }
    Ok(())
}

fn run_command(command: &str, args: &[&str]) -> Result<()> {
    let output = Command::new(command)
        .args(args)
        .output()
        .with_context(|| format!("failed to execute {command}"))?;

    if !output.status.success() {
        bail!(
            "{command} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn install_ca_to_linux_nss(ca_cert_path: &Path, common_name: &str) -> Result<()> {
    let Some(home) = original_user_home_dir() else {
        return Ok(());
    };

    let nss_dir = home.join(".pki").join("nssdb");
    fs::create_dir_all(&nss_dir)
        .with_context(|| format!("failed to create {}", nss_dir.display()))?;

    if command_exists("certutil") {
        let db = format!("sql:{}", nss_dir.display());
        if !nss_dir.join("cert9.db").exists() {
            run_command("certutil", &["-d", &db, "-N", "--empty-password"])?;
        }

        let _ = run_command("certutil", &["-d", &db, "-D", "-n", common_name]);
        run_command(
            "certutil",
            &[
                "-d",
                &db,
                "-A",
                "-t",
                "C,,",
                "-n",
                common_name,
                "-i",
                &ca_cert_path.to_string_lossy(),
            ],
        )?;
        sync_user_ownership(&nss_dir)?;
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn uninstall_ca_from_linux_nss(common_name: &str) -> Result<()> {
    let Some(home) = original_user_home_dir() else {
        return Ok(());
    };

    let nss_dir = home.join(".pki").join("nssdb");
    if !nss_dir.exists() || !command_exists("certutil") {
        return Ok(());
    }

    let db = format!("sql:{}", nss_dir.display());
    let _ = run_command("certutil", &["-d", &db, "-D", "-n", common_name]);
    sync_user_ownership(&nss_dir)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_ca_to_firefox_profiles(ca_cert_path: &Path, common_name: &str) -> Result<()> {
    for profile_db in firefox_profile_dbs()? {
        import_ca_to_nss_db(&profile_db, ca_cert_path, common_name)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn uninstall_ca_from_firefox_profiles(common_name: &str) -> Result<()> {
    for profile_db in firefox_profile_dbs()? {
        remove_ca_from_nss_db(&profile_db, common_name)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn firefox_profile_dbs() -> Result<Vec<PathBuf>> {
    let Some(home) = original_user_home_dir() else {
        return Ok(Vec::new());
    };

    let mut dbs = Vec::new();
    for root in [
        home.join(".mozilla").join("firefox"),
        home.join("snap")
            .join("firefox")
            .join("common")
            .join(".mozilla")
            .join("firefox"),
    ] {
        if !root.exists() {
            continue;
        }

        for entry in
            fs::read_dir(&root).with_context(|| format!("failed to read {}", root.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() && path.join("cert9.db").exists() {
                dbs.push(path);
            }
        }
    }

    Ok(dbs)
}

#[cfg(target_os = "linux")]
fn import_ca_to_nss_db(db_dir: &Path, ca_cert_path: &Path, common_name: &str) -> Result<()> {
    if !command_exists("certutil") {
        return Ok(());
    }

    let db = format!("sql:{}", db_dir.display());
    let _ = run_command("certutil", &["-d", &db, "-D", "-n", common_name]);
    run_command(
        "certutil",
        &[
            "-d",
            &db,
            "-A",
            "-t",
            "C,,",
            "-n",
            common_name,
            "-i",
            &ca_cert_path.to_string_lossy(),
        ],
    )?;
    sync_user_ownership(db_dir)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn remove_ca_from_nss_db(db_dir: &Path, common_name: &str) -> Result<()> {
    if !command_exists("certutil") {
        return Ok(());
    }

    let db = format!("sql:{}", db_dir.display());
    let _ = run_command("certutil", &["-d", &db, "-D", "-n", common_name]);
    sync_user_ownership(db_dir)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn original_user_home_dir() -> Option<PathBuf> {
    let uid = std::env::var("PKEXEC_UID")
        .ok()
        .or_else(|| std::env::var("SUDO_UID").ok())
        .and_then(|value| value.parse::<u32>().ok());

    if let Some(uid) = uid {
        unsafe {
            let passwd = libc::getpwuid(uid);
            if !passwd.is_null() {
                let home = std::ffi::CStr::from_ptr((*passwd).pw_dir);
                return Some(PathBuf::from(home.to_string_lossy().into_owned()));
            }
        }
    }

    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(target_os = "linux")]
fn command_exists(command: &str) -> bool {
    Command::new("which")
        .arg(command)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn run_windows_elevated(executable: &Path, args: &[String]) -> Result<()> {
    let exe = powershell_single_quote(&executable.to_string_lossy());
    let arg_list = if args.is_empty() {
        "@()".to_string()
    } else {
        format!(
            "@({})",
            args.iter()
                .map(|arg| format!("'{}'", powershell_single_quote(arg)))
                .collect::<Vec<_>>()
                .join(",")
        )
    };

    let script =
        format!("Start-Process -FilePath '{exe}' -ArgumentList {arg_list} -Verb RunAs -Wait");
    run_command("powershell", &["-NoProfile", "-Command", &script])
}

fn run_macos_elevated(executable: &Path, args: &[String]) -> Result<()> {
    let command_line = shell_join(executable, args);
    let script = format!(
        "do shell script \"{}\" with administrator privileges",
        applescript_escape(&command_line)
    );
    run_command("osascript", &["-e", &script])
}

fn run_linux_elevated(executable: &Path, args: &[String]) -> Result<()> {
    if Command::new("pkexec")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
    {
        let output = Command::new("pkexec")
            .arg(executable)
            .args(args)
            .output()
            .context("failed to execute pkexec")?;
        if output.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            "pkexec rejected the elevation request".to_string()
        };

        bail!("{detail}");
    }

    bail!("pkexec is required on Linux for GUI elevation prompts")
}

fn shell_join(executable: &Path, args: &[String]) -> String {
    let mut parts = vec![shell_quote(&executable.to_string_lossy())];
    parts.extend(args.iter().map(|arg| shell_quote(arg)));
    parts.join(" ")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn applescript_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn powershell_single_quote(value: &str) -> String {
    value.replace('\'', "''")
}
