#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use linuxdo_accelerator::{gui, service};

fn main() -> anyhow::Result<()> {
    let config_path = service::init_config(None)?;
    gui::run(config_path)
}
