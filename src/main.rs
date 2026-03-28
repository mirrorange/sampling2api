use std::net::SocketAddr;

use anyhow::Context;
use clap::{Parser, Subcommand};
use sampling2api::runtime::{AppState, SamplingBridgeServer};

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Expose MCP client sampling as an Anthropic-compatible Messages API"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Stdio {
        #[arg(long, default_value = "127.0.0.1:38080")]
        listen: SocketAddr,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_target(false)
        .compact()
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Stdio { listen } => {
            let state = AppState::new();
            let bridge = SamplingBridgeServer::stdio(state.peers());
            bridge
                .run_stdio_http_bridge(listen)
                .await
                .with_context(|| format!("stdio bridge failed while serving {listen}"))?;
        }
    }

    Ok(())
}
