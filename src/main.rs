//! mcp-macos binary: serves the MCP server over stdio.

use clap::Parser;
use personai_core::macos::{AppleError, JxaTransport, run_jxa_json};
use rmcp::ServiceExt;
use rmcp::transport::io::stdio;
use serde_json::Value;

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
    /// Comma-separated mail folder allowlist as "Account/Mailbox" entries.
    /// Overrides the config file's mail.folders_allow.
    #[arg(long)]
    mail_folders: Option<String>,
    /// Default sender identity for mail_send ("Name" or email address).
    /// Overrides the config file's mail.default_from.
    #[arg(long)]
    mail_default_from: Option<String>,
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

    let cfg = state_dir.join("mcp-macos.json");
    let (policy, mut warns) = mcp_macos::policy::MailPolicy::load(
        cli.mail_folders.as_deref(),
        cli.mail_default_from.as_deref(),
        &cfg,
    );

    // Degraded mode: if Mail cannot be reached at startup (app closed, TCC
    // denial, …), warn and fall back to an open scope instead of exiting.
    // The server stays usable — non-mail groups are unaffected, and mail
    // tools run without a configured allowlist until restart.
    let mut transport = JxaTransport;
    let scope = match enumerate_folders(&mut transport).await {
        Ok(live) => {
            let (scope, vw) = mcp_macos::policy::EffectiveScope::validate(&policy, &live);
            warns.extend(vw);
            scope
        }
        Err(err) => {
            eprintln!(
                "warning: Mail folder enumeration failed ({err}); starting in degraded \
                 mode with an open mail scope"
            );
            mcp_macos::policy::EffectiveScope::open()
        }
    };
    for w in &warns {
        eprintln!("warning: {w}");
    }

    let server = mcp_macos::MacosServer::new_with_tools(state_dir, enabled, policy, scope);
    let running = server.serve(stdio()).await?;
    running.waiting().await?;
    Ok(())
}

/// Enumerates every Mail account's name together with its mailbox names in
/// one JXA call:
/// `(() => { const M = Application('Mail'); return M.accounts().map(a =>
///   ({ n: a.name(), b: a.mailboxes().map(m => m.name()) })); })()`.
async fn enumerate_folders(t: &mut JxaTransport) -> Result<Vec<(String, Vec<String>)>, AppleError> {
    let v = run_jxa_json(
        t,
        "(() => { const M = Application('Mail'); \
         return M.accounts().map(a => ({ n: a.name(), b: a.mailboxes().map(m => m.name()) })); \
         })()",
    )
    .await?;
    if !v.is_array() {
        return Err(AppleError::Parse(
            "folder enumeration did not return an array".to_string(),
        ));
    }
    let Value::Array(accounts) = v else {
        unreachable!("checked is_array above");
    };
    let mut live = Vec::with_capacity(accounts.len());
    for account in accounts {
        let Some(name) = account.get("n").and_then(Value::as_str) else {
            continue;
        };
        let mailboxes = account
            .get("b")
            .and_then(Value::as_array)
            .map(|mailboxes| {
                mailboxes
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        live.push((name.to_string(), mailboxes));
    }
    Ok(live)
}

fn default_state_dir() -> std::path::PathBuf {
    match std::env::var_os("HOME") {
        Some(home) => std::path::PathBuf::from(home)
            .join(".personai")
            .join("state"),
        None => std::path::PathBuf::from(".personai/state"),
    }
}
