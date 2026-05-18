mod cli;
mod client;
mod config;
pub mod logging;
mod protocol;
mod server;
mod stop;

use anyhow::Result;
use clap::Parser;

use crate::{
    cli::{Cli, Commands},
    client::run_client,
    server::run_server,
    stop::stop_daemon,
};

/// Parses CLI arguments and dispatches the selected mode.
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Server(args) => run_server(args).await,
        Commands::Stop(args) => stop_daemon(args),
        Commands::Exec(args) => run_client(args).await,
    }
}
