fn main() {
    println!("cargo:rerun-if-changed=assets/icons/linuxdo.ico");
    println!("cargo:rerun-if-env-changed=LINUXDO_BUILD_VERSION");
    println!("cargo:rerun-if-env-changed=RELEASE_VERSION");

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

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icons/linuxdo.ico");
        res.compile()
            .expect("failed to compile Windows icon resource");
    }
}
