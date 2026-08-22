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
    /// Comma-separated tool groups to expose
    /// (mail,messages,calendar,notifications,clipboard). Default: all.
    #[arg(long)]
    tools: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let state_dir = cli.state_dir.unwrap_or_else(default_state_dir);
    std::fs::create_dir_all(&state_dir)?;
    let enabled = match cli.tools.as_deref() {
        Some(csv) => mcp_macos::EnabledTools::parse(csv).map_err(anyhow::Error::msg)?,
        None => match std::env::var("MCP_MACOS_TOOLS") {
            Ok(csv) => mcp_macos::EnabledTools::parse(&csv).map_err(anyhow::Error::msg)?,
            Err(_) => mcp_macos::EnabledTools::all(),
        },
    };

    let server = mcp_macos::MacosServer::new_with_tools(state_dir, enabled);
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
