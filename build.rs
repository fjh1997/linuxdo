fn main() {
    println!("cargo:rerun-if-changed=assets/icons/linuxdo.ico");
    println!("cargo:rerun-if-env-changed=LINUXDO_BUILD_VERSION");
    println!("cargo:rerun-if-env-changed=RELEASE_VERSION");
    println!("cargo:rerun-if-env-changed=LINUXDO_GIT_HASH");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs");

    let build_version = std::env::var("LINUXDO_BUILD_VERSION")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("RELEASE_VERSION")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .or_else(|| {
            std::env::var("CARGO_PKG_VERSION")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| "0.0.0".to_string());
    println!("cargo:rustc-env=LINUXDO_BUILD_VERSION={build_version}");

    let git_hash = std::env::var("LINUXDO_GIT_HASH")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            std::process::Command::new("git")
                .args(["rev-parse", "--short=12", "HEAD"])
                .output()
                .ok()
                .filter(|output| output.status.success())
                .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| build_version.clone())
        });
    println!("cargo:rustc-env=LINUXDO_GIT_HASH={git_hash}");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icons/linuxdo.ico");
        res.compile()
            .expect("failed to compile Windows icon resource");
    }
}
