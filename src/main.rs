use clap::{Parser, Subcommand};
use anyhow::Result;

mod commands;
mod core;

#[derive(Parser)]
#[command(name = "dotest")]
#[command(version)]
#[command(about = "A fast, minimal, and ergonomic terminal tool for running .NET tests", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Open interactive UI mode
    Ui,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Commands::Ui => {
            commands::ui::run()?;
        }
    }

    Ok(())
}
