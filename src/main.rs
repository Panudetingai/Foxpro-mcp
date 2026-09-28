mod backup;
mod cli;
mod code;
mod config;
mod error;
mod mcp;
mod sandbox;
mod vfp;

use clap::Parser;
use cli::Args;
use config::Config;
use mcp::Server;

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let config = match Config::load(
        args.config,
        args.workspace,
        args.log_level,
        args.vfp_path,
        args.vfp_timeout,
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to load configuration: {e}");
            std::process::exit(1);
        }
    };

    let env_filter = tracing_subscriber::EnvFilter::try_new(&config.log_level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(std::io::stderr)
        .init();

    tracing::debug!(workspace = %config.workspace.display(), "starting server");

    let server = match Server::new(config) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("Failed to create server: {e}");
            std::process::exit(1);
        }
    };

    if let Err(e) = server.run().await {
        tracing::error!("Server error: {e}");
        std::process::exit(1);
    }
}
