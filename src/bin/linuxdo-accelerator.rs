use clap::Parser;
use linuxdo_accelerator::cli::{self, Cli};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    cli::run(cli).await
}
