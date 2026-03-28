use std::net::SocketAddr;

use anyhow::Context;
use clap::{Parser, Subcommand};
use sampling2api::runtime::{run_http_bridge, run_stdio_bridge};
use tracing::Level;

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
    Http {
        #[arg(long, default_value = "127.0.0.1:38080")]
        listen: SocketAddr,
        #[arg(long, default_value = "/mcp")]
        mcp_path: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_target(false)
        .with_max_level(Level::WARN)
        .compact()
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Stdio { listen } => {
            run_stdio_bridge(listen)
                .await
                .with_context(|| format!("stdio bridge failed while serving {listen}"))?;
        }
        Command::Http { listen, mcp_path } => {
            run_http_bridge(listen, &mcp_path).await.with_context(|| {
                format!(
                    "streamable HTTP bridge failed while serving {listen} with MCP path {mcp_path}"
                )
            })?;
        }
    }

    Ok(())
}
