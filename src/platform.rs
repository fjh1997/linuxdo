#[cfg(target_os = "windows")]
use std::ffi::OsString;
use std::fs;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use md5::compute as md5_compute;
use x509_parser::prelude::{FromDer, X509Certificate};

use crate::config::AppConfig;

#[cfg(target_os = "windows")]
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, HANDLE, HWND, INVALID_HANDLE_VALUE, STILL_ACTIVE, WAIT_FAILED,
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Console::{
    ATTACH_PARENT_PROCESS, AttachConsole, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    SetStdHandle,
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, INFINITE, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    WaitForSingleObject,
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
#[cfg(target_os = "windows")]
use windows_sys::Win32::UI::WindowsAndMessaging::{
    ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_DEFAULTSIZE, LR_SHARED, LoadImageW, PostMessageW, SW_HIDE,
    SW_NORMAL, SW_RESTORE, SW_SHOW, SendMessageW, SetForegroundWindow, ShowWindow, WM_CLOSE,
    WM_SETICON,
};

const LINUX_CA_FILE_NAME: &str = "linuxdo-accelerator-root-ca.crt";

enum MacosCaState {
    Missing,
    Matching,
    Mismatched,
}

pub fn ensure_elevated(config: &AppConfig, for_setup: bool) -> Result<()> {
    let needs_privilege = for_setup || config.http_port < 1024 || config.https_port < 1024;
    if needs_privilege && !is_elevated() {
        bail!("this command requires administrator/root privileges");
    }
    Ok(())
}

pub fn is_elevated() -> bool {
    if cfg!(target_os = "windows") {
        let mut command = Command::new("fltmc");
        configure_hidden_windows_command(&mut command);
        return command
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
        "android" => run_android_elevated(executable, args),
        _ => run_linux_elevated(executable, args),
    }
}

#[cfg(target_os = "windows")]
pub fn prepare_windows_cli_stdio(args: &[OsString]) {
    if args.len() <= 1 {
        return;
    }

    let attached = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
    if attached == 0 {
        return;
    }

    let input = open_windows_console_device("CONIN$", FILE_GENERIC_READ | FILE_GENERIC_WRITE);
    if input != INVALID_HANDLE_VALUE {
        unsafe {
            let _ = SetStdHandle(STD_INPUT_HANDLE, input);
        }
    }

    let output = open_windows_console_device("CONOUT$", FILE_GENERIC_READ | FILE_GENERIC_WRITE);
    if output != INVALID_HANDLE_VALUE {
        unsafe {
            let _ = SetStdHandle(STD_OUTPUT_HANDLE, output);
            let _ = SetStdHandle(STD_ERROR_HANDLE, output);
        }
    }
}

#[cfg(not(target_os = "windows"))]
pub fn prepare_windows_cli_stdio<T>(_args: &[T]) {}

#[cfg(target_os = "windows")]
fn open_windows_console_device(name: &str, access: u32) -> HANDLE {
    let wide = wide_null(name);
    unsafe {
        CreateFileW(
            wide.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    }
}

pub fn spawn_detached(executable: &Path, args: &[String]) -> Result<u32> {
    #[cfg(target_os = "linux")]
    {
        let mut command = Command::new("setsid");
        command
            .arg(executable)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let child = command
            .spawn()
            .with_context(|| format!("failed to spawn detached {}", executable.display()))?;
        return Ok(child.id());
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
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            command.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
        }

        let child = command
            .spawn()
            .with_context(|| format!("failed to spawn detached {}", executable.display()))?;
        let initial_pid = child.id();

        #[cfg(target_os = "windows")]
        {
            let executable_name = executable
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| anyhow!("invalid executable name {}", executable.display()))?;
            if let Some(resolved_pid) = find_spawned_windows_child(initial_pid, executable_name)? {
                return Ok(resolved_pid);
            }
        }

        Ok(initial_pid)
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
        #[cfg(target_os = "windows")]
        {
            return is_windows_process_running(pid);
        }
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

pub fn ensure_loopback_alias(config: &AppConfig) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        ensure_macos_loopback_aliases(config)?;
    }

    Ok(())
}

pub fn remove_loopback_alias(config: &AppConfig) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        remove_macos_loopback_aliases(config)?;
    }

    Ok(())
}

#[cfg(target_os = "windows")]
fn is_windows_process_running(pid: u32) -> bool {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let handle = HandleGuard(handle);

    let mut exit_code = 0u32;
    let ok = unsafe { GetExitCodeProcess(handle.0, &mut exit_code) };
    ok != 0 && exit_code == STILL_ACTIVE as u32
}

pub fn install_ca(ca_cert_path: &Path, common_name: &str) -> Result<()> {
    if !cfg!(target_os = "macos") {
        let _ = uninstall_ca(common_name);
    }

    match std::env::consts::OS {
        "windows" => {
            run_command(
                "certutil",
                &["-f", "-addstore", "Root", &ca_cert_path.to_string_lossy()],
            )?;
        }
        "macos" => {
            match macos_ca_state(ca_cert_path, common_name)? {
                MacosCaState::Matching => {
                    println!("root certificate already installed: {common_name}");
                    return Ok(());
                }
                MacosCaState::Missing => {}
                MacosCaState::Mismatched => {
                    let _ = uninstall_ca(common_name);
                }
            }
            let keychain = macos_install_keychain_path();
            let keychain = keychain.to_string_lossy().into_owned();
            let cert = ca_cert_path.to_string_lossy().into_owned();
            let args = if keychain == "/Library/Keychains/System.keychain" {
                vec![
                    "add-trusted-cert",
                    "-d",
                    "-r",
                    "trustRoot",
                    "-k",
                    &keychain,
                    &cert,
                ]
            } else {
                vec![
                    "add-trusted-cert",
                    "-r",
                    "trustRoot",
                    "-k",
                    &keychain,
                    &cert,
                ]
            };
            run_command("security", &args)?;
        }
        "android" => {
            android_install_ca(ca_cert_path, common_name)?;
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

#[cfg(target_os = "macos")]
fn ensure_macos_loopback_aliases(config: &AppConfig) -> Result<()> {
    for addr in macos_loopback_aliases(config) {
        let output = Command::new("ifconfig")
            .args(["lo0", "alias", &addr, "up"])
            .output()
            .with_context(|| format!("failed to execute ifconfig for loopback alias {addr}"))?;
        if output.status.success() {
            continue;
        }

        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.contains("File exists") {
            continue;
        }

        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            "unknown error".to_string()
        };
        bail!("ifconfig lo0 alias {addr} failed: {detail}");
    }

    Ok(())
}

#[cfg(target_os = "macos")]
fn remove_macos_loopback_aliases(config: &AppConfig) -> Result<()> {
    for addr in macos_loopback_aliases(config) {
        let output = Command::new("ifconfig")
            .args(["lo0", "-alias", &addr])
            .output()
            .with_context(|| format!("failed to execute ifconfig for loopback alias cleanup {addr}"))?;
        if output.status.success() {
            continue;
        }

        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.contains("Can't assign requested address") || stderr.contains("does not exist") {
            continue;
        }

        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            "unknown error".to_string()
        };
        bail!("ifconfig lo0 -alias {addr} failed: {detail}");
    }

    Ok(())
}

#[cfg(target_os = "macos")]
fn macos_loopback_aliases(config: &AppConfig) -> Vec<String> {
    let mut addrs = Vec::new();
    for candidate in [&config.listen_host, &config.hosts_ip] {
        if let Ok(ip) = candidate.parse::<Ipv4Addr>() {
            if ip.octets()[0] == 127 && ip != Ipv4Addr::new(127, 0, 0, 1) {
                let addr = candidate.to_string();
                if !addrs.contains(&addr) {
                    addrs.push(addr);
                }
            }
        }
    }
    addrs
}

pub fn uninstall_ca(common_name: &str) -> Result<()> {
    match std::env::consts::OS {
        "windows" => {
            run_command("certutil", &["-delstore", "Root", common_name])?;
        }
        "macos" => {
            let _ = run_command(
                "security",
                &[
                    "delete-certificate",
                    "-c",
                    common_name,
                    "/Library/Keychains/System.keychain",
                ],
            );
            if let Some(login_keychain) = macos_login_keychain_path() {
                let login_keychain = login_keychain.to_string_lossy().into_owned();
                let _ = run_command(
                    "security",
                    &["delete-certificate", "-c", common_name, &login_keychain],
                );
            }
        }
        "android" => {
            android_uninstall_ca(common_name)?;
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

#[cfg(target_os = "macos")]
fn macos_ca_state(ca_cert_path: &Path, common_name: &str) -> Result<MacosCaState> {
    let expected_hash = certificate_sha256(ca_cert_path)?;
    let system_hashes =
        macos_keychain_cert_hashes(common_name, Path::new("/Library/Keychains/System.keychain"))?;
    let login_hashes = if let Some(login_keychain) = macos_login_keychain_path() {
        macos_keychain_cert_hashes(common_name, &login_keychain)?
    } else {
        Vec::new()
    };

    let trusted = macos_ca_trusted(ca_cert_path)?;
    let system_matches =
        !system_hashes.is_empty() && system_hashes.iter().all(|h| h == &expected_hash);
    let login_matches =
        !login_hashes.is_empty() && login_hashes.iter().all(|h| h == &expected_hash);
    let any_mismatch = system_hashes
        .iter()
        .chain(login_hashes.iter())
        .any(|hash| hash != &expected_hash);

    if trusted && (system_matches || login_matches) {
        return Ok(MacosCaState::Matching);
    }
    if any_mismatch {
        return Ok(MacosCaState::Mismatched);
    }
    Ok(MacosCaState::Missing)
}

#[cfg(not(target_os = "macos"))]
fn macos_ca_state(_ca_cert_path: &Path, _common_name: &str) -> Result<MacosCaState> {
    Ok(MacosCaState::Missing)
}

#[cfg(target_os = "macos")]
fn macos_login_keychain_path() -> Option<PathBuf> {
    let output = Command::new("stat")
        .args(["-f", "%Su", "/dev/console"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let user = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if user.is_empty() || user == "root" {
        return None;
    }
    Some(
        PathBuf::from("/Users")
            .join(user)
            .join("Library")
            .join("Keychains")
            .join("login.keychain-db"),
    )
}

#[cfg(not(target_os = "macos"))]
fn macos_login_keychain_path() -> Option<PathBuf> {
    None
}

#[cfg(target_os = "macos")]
fn macos_install_keychain_path() -> PathBuf {
    macos_login_keychain_path()
        .unwrap_or_else(|| PathBuf::from("/Library/Keychains/System.keychain"))
}

#[cfg(not(target_os = "macos"))]
fn macos_install_keychain_path() -> PathBuf {
    PathBuf::new()
}

#[cfg(target_os = "macos")]
fn certificate_sha256(path: &Path) -> Result<String> {
    let output = Command::new("openssl")
        .args([
            "x509",
            "-in",
            &path.to_string_lossy(),
            "-noout",
            "-fingerprint",
            "-sha256",
        ])
        .output()
        .with_context(|| format!("failed to fingerprint {}", path.display()))?;
    if !output.status.success() {
        bail!(
            "openssl fingerprint failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let fingerprint = stdout
        .lines()
        .find_map(|line| line.split_once('='))
        .map(|(_, value)| value.trim().replace(':', "").to_ascii_uppercase())
        .ok_or_else(|| {
            anyhow!(
                "failed to parse certificate fingerprint for {}",
                path.display()
            )
        })?;
    Ok(fingerprint)
}

#[cfg(target_os = "macos")]
fn parse_security_sha256_hashes(output: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(output)
        .lines()
        .filter_map(|line| line.strip_prefix("SHA-256 hash: "))
        .map(|hash| hash.trim().replace(':', "").to_ascii_uppercase())
        .collect()
}

#[cfg(target_os = "macos")]
fn macos_keychain_cert_hashes(common_name: &str, keychain: &Path) -> Result<Vec<String>> {
    let keychain = keychain.to_string_lossy().into_owned();
    let output = Command::new("security")
        .args(["find-certificate", "-a", "-c", common_name, "-Z", &keychain])
        .output()
        .with_context(|| format!("failed to query {keychain}"))?;
    if !output.status.success() || output.stdout.is_empty() {
        return Ok(Vec::new());
    }
    Ok(parse_security_sha256_hashes(&output.stdout))
}

#[cfg(target_os = "macos")]
fn macos_ca_trusted(ca_cert_path: &Path) -> Result<bool> {
    let output = Command::new("security")
        .args([
            "verify-cert",
            "-c",
            &ca_cert_path.to_string_lossy(),
            "-p",
            "basic",
        ])
        .output()
        .with_context(|| format!("failed to verify trust for {}", ca_cert_path.display()))?;
    Ok(output.status.success())
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

#[cfg(target_os = "windows")]
pub fn update_windows_shortcuts_for_exe(executable: &Path) -> Result<()> {
    let script = format!(
        r#"
$exe = '{exe}'
$roots = @(
  [Environment]::GetFolderPath('Desktop'),
  [Environment]::GetFolderPath('CommonDesktopDirectory'),
  [Environment]::GetFolderPath('Programs'),
  [Environment]::GetFolderPath('CommonPrograms')
) | Where-Object {{ $_ -and (Test-Path $_) }} | Select-Object -Unique
$shell = New-Object -ComObject WScript.Shell
foreach ($root in $roots) {{
  Get-ChildItem -Path $root -Filter '*.lnk' -Recurse -ErrorAction SilentlyContinue | ForEach-Object {{
    try {{
      $shortcut = $shell.CreateShortcut($_.FullName)
      if ($shortcut.TargetPath -and $shortcut.TargetPath.ToLowerInvariant() -eq $exe.ToLowerInvariant()) {{
        $shortcut.IconLocation = "$exe,0"
        $shortcut.Save()
      }}
    }} catch {{}}
  }}
}}
if (Get-Command ie4uinit.exe -ErrorAction SilentlyContinue) {{
  Start-Process ie4uinit.exe -ArgumentList '-show' -WindowStyle Hidden -Wait -ErrorAction SilentlyContinue
}}
"#,
        exe = powershell_literal(executable),
    );

    run_powershell_file(&script)
}

#[cfg(target_os = "windows")]
pub fn hide_app_window(hwnd: isize) -> Result<()> {
    let hwnd = hwnd as HWND;
    let shown = unsafe { ShowWindow(hwnd, SW_HIDE) };
    let _ = shown;
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn restore_app_window(hwnd: isize) -> Result<()> {
    let hwnd = hwnd as HWND;
    unsafe {
        ShowWindow(hwnd, SW_SHOW);
        ShowWindow(hwnd, SW_RESTORE);
        SetForegroundWindow(hwnd);
    }
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn close_app_window(hwnd: isize) -> Result<()> {
    let hwnd = hwnd as HWND;
    let posted = unsafe { PostMessageW(hwnd, WM_CLOSE, 0, 0) };
    if posted == 0 {
        return Err(std::io::Error::last_os_error()).context("failed to post close message");
    }
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn apply_app_window_icon(hwnd: isize) -> Result<()> {
    let hwnd = hwnd as HWND;
    let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
    if instance.is_null() {
        return Err(std::io::Error::last_os_error()).context("failed to get module handle");
    }

    let flags = LR_DEFAULTSIZE | LR_SHARED;
    let resource = 1usize as *const u16;
    let small_icon = unsafe { LoadImageW(instance, resource, IMAGE_ICON, 32, 32, flags) };
    let big_icon = unsafe { LoadImageW(instance, resource, IMAGE_ICON, 256, 256, flags) };

    if small_icon.is_null() || big_icon.is_null() {
        return Err(std::io::Error::last_os_error()).context("failed to load icon resource");
    }

    unsafe {
        SendMessageW(hwnd, WM_SETICON, ICON_SMALL as usize, small_icon as isize);
        SendMessageW(hwnd, WM_SETICON, ICON_BIG as usize, big_icon as isize);
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
    let mut child = Command::new(command);
    child.args(args);
    configure_hidden_windows_command(&mut child);
    let output = child
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

#[cfg(target_os = "android")]
fn android_install_ca(ca_cert_path: &Path, common_name: &str) -> Result<()> {
    let cert_pem = fs::read(ca_cert_path)
        .with_context(|| format!("failed to read {}", ca_cert_path.display()))?;
    let hash = android_cert_subject_hash_old(&cert_pem)?;
    let file_name = format!("{hash}.0");
    let destinations = android_ca_store_destinations(&file_name);
    let mut installed = false;

    for dest in destinations {
        if let Some(parent) = dest.parent() {
            let _ = fs::create_dir_all(parent);
        }
        match fs::write(&dest, &cert_pem) {
            Ok(_) => {
                android_fixup_ca_permissions(&dest)?;
                installed = true;
            }
            Err(_) => continue,
        }
    }

    if !installed {
        bail!("failed to install Android CA into any known trust store");
    }

    println!("installed root certificate: {common_name}");
    Ok(())
}

#[cfg(not(target_os = "android"))]
fn android_install_ca(_ca_cert_path: &Path, _common_name: &str) -> Result<()> {
    bail!("android certificate installation is unavailable on this platform")
}

#[cfg(target_os = "android")]
fn android_uninstall_ca(common_name: &str) -> Result<()> {
    let mut removed = false;
    for store in android_ca_store_dirs() {
        if !store.exists() {
            continue;
        }
        for entry in fs::read_dir(&store)
            .with_context(|| format!("failed to read directory {}", store.display()))?
        {
            let entry = entry.with_context(|| format!("failed to walk {}", store.display()))?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let cert_pem = match fs::read(&path) {
                Ok(value) => value,
                Err(_) => continue,
            };
            let parsed_cn = match android_cert_common_name(&cert_pem) {
                Ok(value) => value,
                Err(_) => continue,
            };
            if parsed_cn == common_name {
                fs::remove_file(&path)
                    .with_context(|| format!("failed to remove {}", path.display()))?;
                removed = true;
            }
        }
    }

    if !removed {
        println!("root certificate not found in Android trust store: {common_name}");
    }
    Ok(())
}

#[cfg(not(target_os = "android"))]
fn android_uninstall_ca(_common_name: &str) -> Result<()> {
    bail!("android certificate removal is unavailable on this platform")
}

#[cfg(target_os = "android")]
fn android_ca_store_dirs() -> Vec<PathBuf> {
    let candidates = [
        "/apex/com.android.conscrypt/cacerts",
        "/system/etc/security/cacerts",
    ];
    candidates
        .iter()
        .map(PathBuf::from)
        .filter(|path| path.exists())
        .collect()
}

#[cfg(target_os = "android")]
fn android_ca_store_destinations(file_name: &str) -> Vec<PathBuf> {
    android_ca_store_dirs()
        .into_iter()
        .map(|dir| dir.join(file_name))
        .collect()
}

#[cfg(target_os = "android")]
fn android_fixup_ca_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let permissions = fs::Permissions::from_mode(0o644);
    fs::set_permissions(path, permissions)
        .with_context(|| format!("failed to chmod {}", path.display()))?;
    Ok(())
}

fn android_cert_subject_hash_old(cert_pem: &[u8]) -> Result<String> {
    let mut reader = std::io::BufReader::new(cert_pem);
    let mut certs = rustls_pemfile::certs(&mut reader);
    let der = certs
        .next()
        .transpose()
        .context("failed to parse PEM certificate")?
        .ok_or_else(|| anyhow!("certificate PEM is empty"))?;
    let (_, cert) = X509Certificate::from_der(der.as_ref())
        .map_err(|error| anyhow!("failed to parse X509 certificate: {error}"))?;
    let digest = md5_compute(cert.tbs_certificate.subject.as_raw());
    let value = u32::from_le_bytes([digest.0[0], digest.0[1], digest.0[2], digest.0[3]]);
    Ok(format!("{value:08x}"))
}

fn android_cert_common_name(cert_pem: &[u8]) -> Result<String> {
    let mut reader = std::io::BufReader::new(cert_pem);
    let mut certs = rustls_pemfile::certs(&mut reader);
    let der = certs
        .next()
        .transpose()
        .context("failed to parse PEM certificate")?
        .ok_or_else(|| anyhow!("certificate PEM is empty"))?;
    let (_, cert) = X509Certificate::from_der(der.as_ref())
        .map_err(|error| anyhow!("failed to parse X509 certificate: {error}"))?;
    let subject = cert.subject();
    let attribute = subject
        .iter_common_name()
        .next()
        .ok_or_else(|| anyhow!("certificate common name is missing"))?;
    let cn = attribute
        .as_str()
        .map_err(|error| anyhow!("failed to decode certificate common name: {error}"))?;
    Ok(cn.to_string())
}

#[cfg(target_os = "windows")]
fn configure_hidden_windows_command(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(target_os = "windows"))]
fn configure_hidden_windows_command(_command: &mut Command) {}

#[cfg(target_os = "windows")]
fn run_powershell_file(script: &str) -> Result<()> {
    let script_path = std::env::temp_dir().join(format!(
        "linuxdo-shortcut-refresh-{}.ps1",
        std::process::id()
    ));
    fs::write(&script_path, script)
        .with_context(|| format!("failed to write {}", script_path.display()))?;

    let result = (|| -> Result<()> {
        let mut command = Command::new("powershell");
        command.args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ]);
        command.arg(&script_path);
        configure_hidden_windows_command(&mut command);
        let output = command.output().context("failed to run powershell")?;
        if !output.status.success() {
            bail!(
                "powershell failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    })();

    let _ = fs::remove_file(&script_path);
    result
}

#[cfg(target_os = "windows")]
fn powershell_literal(path: &Path) -> String {
    path.to_string_lossy().replace('\'', "''")
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

#[cfg(target_os = "windows")]
fn run_windows_elevated(executable: &Path, args: &[String]) -> Result<()> {
    let exe_wide = wide_null(executable.as_os_str());
    let verb_wide = wide_null("runas");
    let params = windows_command_line(args);
    let params_wide = wide_null(&params);

    let mut execute_info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        hwnd: std::ptr::null_mut::<std::ffi::c_void>() as HWND,
        lpVerb: verb_wide.as_ptr(),
        lpFile: exe_wide.as_ptr(),
        lpParameters: params_wide.as_ptr(),
        lpDirectory: std::ptr::null(),
        nShow: SW_NORMAL,
        ..unsafe { std::mem::zeroed() }
    };

    let launched = unsafe { ShellExecuteExW(&mut execute_info) };
    if launched == 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "failed to elevate {} with ShellExecuteExW",
                executable.display()
            )
        });
    }

    if execute_info.hProcess.is_null() {
        bail!("ShellExecuteExW did not return a process handle");
    }

    let process = HandleGuard(execute_info.hProcess);
    let wait_result = unsafe { WaitForSingleObject(process.0, INFINITE) };
    if wait_result == WAIT_FAILED {
        return Err(std::io::Error::last_os_error())
            .context("failed while waiting for elevated process");
    }

    let mut exit_code = 0u32;
    let exit_code_ok = unsafe { GetExitCodeProcess(process.0, &mut exit_code) };
    if exit_code_ok == 0 {
        return Err(std::io::Error::last_os_error())
            .context("failed to fetch elevated process exit code");
    }

    if exit_code != 0 {
        bail!("elevated command exited with code {exit_code}");
    }

    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn run_windows_elevated(_executable: &Path, _args: &[String]) -> Result<()> {
    bail!("windows elevation is unavailable on this platform")
}

#[cfg(target_os = "windows")]
fn find_spawned_windows_child(parent_pid: u32, executable_name: &str) -> Result<Option<u32>> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);

    while std::time::Instant::now() < deadline {
        if let Some(pid) = snapshot_windows_child_pid(parent_pid, executable_name)? {
            return Ok(Some(pid));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    Ok(None)
}

#[cfg(target_os = "macos")]
fn run_macos_elevated(executable: &Path, args: &[String]) -> Result<()> {
    let command_line = shell_join(executable, args);
    let prompt =
        "Linux.do Accelerator 需要管理员权限来安装证书、更新 hosts 并监听本地 80/443 端口。";
    let script = format!(
        "do shell script \"{}\" with prompt \"{}\" with administrator privileges",
        applescript_escape(&command_line),
        applescript_escape(prompt),
    );
    match run_command("osascript", &["-e", &script]) {
        Ok(()) => Ok(()),
        Err(error) if should_fallback_to_macos_sudo(&error.to_string()) => {
            run_macos_sudo_prompt(executable, args)
                .with_context(|| format!("AppleScript elevation failed: {error}"))
        }
        Err(error) => Err(error),
    }
}

#[cfg(not(target_os = "macos"))]
fn run_macos_elevated(_executable: &Path, _args: &[String]) -> Result<()> {
    bail!("macos elevation is unavailable on this platform")
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

#[cfg(target_os = "android")]
fn run_android_elevated(executable: &Path, args: &[String]) -> Result<()> {
    let mut command_line = shell_quote_arg(&executable.to_string_lossy());
    for arg in args {
        command_line.push(' ');
        command_line.push_str(&shell_quote_arg(arg));
    }

    let output = Command::new("su")
        .args(["-c", &command_line])
        .output()
        .context("failed to execute su")?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        bail!("su rejected the elevation request");
    }
    bail!("su failed: {stderr}");
}

#[cfg(not(target_os = "android"))]
fn run_android_elevated(_executable: &Path, _args: &[String]) -> Result<()> {
    bail!("android elevation is unavailable on this platform")
}

fn shell_quote_arg(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
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

#[cfg(target_os = "macos")]
fn should_fallback_to_macos_sudo(detail: &str) -> bool {
    detail.contains("-60007")
        || detail.contains("-50")
        || detail.contains("管理员用户名或密码不正确")
        || detail.contains("authorization failed")
        || detail.contains("授权失败")
}

#[cfg(not(target_os = "macos"))]
fn should_fallback_to_macos_sudo(_detail: &str) -> bool {
    false
}

#[cfg(target_os = "macos")]
fn run_macos_sudo_prompt(executable: &Path, args: &[String]) -> Result<()> {
    let password = prompt_macos_administrator_password()?;
    let mut command = Command::new("sudo");
    command
        .arg("-k")
        .arg("-S")
        .arg("-p")
        .arg("")
        .arg("--")
        .arg(executable)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to execute sudo for {}", executable.display()))?;

    {
        use std::io::Write;

        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("failed to open sudo stdin"))?;
        stdin
            .write_all(password.as_bytes())
            .context("failed to write password to sudo stdin")?;
        stdin
            .write_all(b"\n")
            .context("failed to terminate sudo password input")?;
        stdin
            .flush()
            .context("failed to flush sudo password input")?;
    }

    let output = child
        .wait_with_output()
        .context("failed while waiting for sudo")?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        bail!("sudo rejected the administrator password");
    }

    bail!("{stderr}");
}

#[cfg(target_os = "macos")]
fn prompt_macos_administrator_password() -> Result<String> {
    let script = format!(
        "text returned of (display dialog \"{}\" with title \"Linux.do Accelerator\" default answer \"\" with hidden answer buttons {{\"取消\", \"继续\"}} default button \"继续\")",
        applescript_escape(
            "请输入 macOS 管理员密码，用于安装证书、更新 hosts 并监听本地 80/443 端口。"
        ),
    );
    let output = Command::new("osascript")
        .args(["-e", &script])
        .output()
        .context("failed to prompt for administrator password")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("-128") || stderr.contains("User canceled") || stderr.contains("取消")
        {
            bail!("user canceled administrator password prompt");
        }
        bail!("password prompt failed: {}", stderr.trim());
    }

    let password = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if password.is_empty() {
        bail!("empty administrator password");
    }

    Ok(password)
}

#[cfg(target_os = "windows")]
fn snapshot_windows_child_pid(parent_pid: u32, executable_name: &str) -> Result<Option<u32>> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error())
            .context("failed to snapshot Windows process list");
    }
    let snapshot = HandleGuard(snapshot);

    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..unsafe { std::mem::zeroed() }
    };

    let mut ok = unsafe { Process32FirstW(snapshot.0, &mut entry) };
    while ok != 0 {
        if entry.th32ParentProcessID == parent_pid
            && windows_process_name(&entry).eq_ignore_ascii_case(executable_name)
        {
            return Ok(Some(entry.th32ProcessID));
        }
        ok = unsafe { Process32NextW(snapshot.0, &mut entry) };
    }

    let last_error = unsafe { GetLastError() };
    if last_error != 0 && last_error != 18 {
        return Err(std::io::Error::from_raw_os_error(last_error as i32))
            .context("failed to iterate Windows process list");
    }

    Ok(None)
}

#[cfg(target_os = "windows")]
fn windows_process_name(entry: &PROCESSENTRY32W) -> String {
    let len = entry
        .szExeFile
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(entry.szExeFile.len());
    String::from_utf16_lossy(&entry.szExeFile[..len])
}

#[cfg(target_os = "windows")]
fn wide_null(value: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    value.as_ref().encode_wide().chain(Some(0)).collect()
}

#[cfg(target_os = "windows")]
fn windows_command_line(args: &[String]) -> String {
    args.iter()
        .map(|arg| windows_quote_arg(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(target_os = "windows")]
fn windows_quote_arg(value: &str) -> String {
    if value.is_empty() {
        return "\"\"".to_string();
    }

    let needs_quotes = value.chars().any(|ch| matches!(ch, ' ' | '\t' | '"'));
    if !needs_quotes {
        return value.to_string();
    }

    let mut result = String::from("\"");
    let mut backslashes = 0;
    for ch in value.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                result.push_str(&"\\".repeat(backslashes * 2 + 1));
                result.push('"');
                backslashes = 0;
            }
            _ => {
                if backslashes > 0 {
                    result.push_str(&"\\".repeat(backslashes));
                    backslashes = 0;
                }
                result.push(ch);
            }
        }
    }
    if backslashes > 0 {
        result.push_str(&"\\".repeat(backslashes * 2));
    }
    result.push('"');
    result
}

#[cfg(target_os = "windows")]
struct HandleGuard(HANDLE);

#[cfg(target_os = "windows")]
impl Drop for HandleGuard {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}
