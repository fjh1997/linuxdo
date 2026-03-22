use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::gui;
use crate::service;

#[derive(Debug, Parser)]
#[command(name = "linuxdo-accelerator")]
#[command(about = "linux.do accelerator CLI")]
pub struct Cli {
    #[arg(long)]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Gui,
    InitConfig,
    Setup,
    Run,
    Start,
    Stop,
    Status,
    CleanHosts,
    ApplyHosts,
    UninstallCert,
    Cleanup,
    #[command(hide = true)]
    HelperStart,
    #[command(hide = true)]
    HelperStop,
    #[command(hide = true)]
    Daemon,
}

pub async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        None | Some(Command::Gui) => {
            let config_path = service::init_config(cli.config.clone())?;
            gui::run(config_path)?;
        }
        Some(Command::InitConfig) => {
            let config_path = service::init_config(cli.config)?;
            println!("config ready: {}", config_path.display());
        }
        Some(Command::Setup) => {
            service::setup(cli.config)?;
            println!("setup complete");
        }
        Some(Command::Run) => {
            service::run_foreground(cli.config, false).await?;
        }
        Some(Command::Start) => {
            service::run_foreground(cli.config, true).await?;
        }
        Some(Command::Stop) | Some(Command::HelperStop) => {
            service::helper_stop(cli.config)?;
            println!("service stopped");
        }
        Some(Command::Status) => {
            let status = service::status(cli.config)?;
            println!("{}", serde_json::to_string_pretty(&status)?);
        }
        Some(Command::CleanHosts) => {
            service::clean_hosts(cli.config)?;
            println!("hosts cleaned");
        }
        Some(Command::ApplyHosts) => {
            service::apply_hosts_only(cli.config)?;
            println!("hosts applied");
        }
        Some(Command::UninstallCert) => {
            service::uninstall_certificate(cli.config)?;
            println!("certificate removed");
        }
        Some(Command::Cleanup) => {
            service::cleanup(cli.config)?;
            println!("cleanup complete");
        }
        Some(Command::HelperStart) => {
            service::helper_start(cli.config)?;
            println!("service started");
        }
        Some(Command::Daemon) => {
            service::run_foreground(cli.config, false).await?;
        }
    }

    Ok(())
}
