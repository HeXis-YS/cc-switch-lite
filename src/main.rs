use cc_switch_lite::{config::Config, server};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
#[derive(Parser)]
#[command(version, about = "Responses proxy with model-based provider routing")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Serve {
        #[arg(long)]
        config: PathBuf,
    },
    Validate {
        #[arg(long)]
        config: PathBuf,
    },
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Serve { config } => server::serve(Config::load(&config)?).await,
        Command::Validate { config } => {
            Config::load(&config)?;
            println!("Configuration valid");
            Ok(())
        }
    }
}
