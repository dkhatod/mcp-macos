//! mcp-macos binary: serves the MCP server over stdio.

use clap::Parser;
use rmcp::ServiceExt;
use rmcp::transport::io::stdio;

#[derive(Parser)]
#[command(
    name = "mcp-macos",
    about = "MCP server exposing Apple Mail, Messages, Calendar, Notifications and Clipboard",
    version
)]
struct Cli {
    /// State directory (token store). Default: ~/.personai/state
    #[arg(long)]
    state_dir: Option<std::path::PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let state_dir = cli.state_dir.unwrap_or_else(default_state_dir);
    std::fs::create_dir_all(&state_dir)?;

    let server = mcp_macos::MacosServer::new(state_dir);
    let running = server.serve(stdio()).await?;
    running.waiting().await?;
    Ok(())
}

fn default_state_dir() -> std::path::PathBuf {
    match std::env::var_os("HOME") {
        Some(home) => std::path::PathBuf::from(home)
            .join(".personai")
            .join("state"),
        None => std::path::PathBuf::from(".personai/state"),
    }
}
