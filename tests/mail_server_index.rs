//! Contract tests for server-side index wiring: `source` validation on
//! `mail_search` and `mail_sync` parameter/scope validation. Live transport
//! paths are covered by the user's acceptance runs; these pin the branches
//! that must never reach osascript.

use mcp_macos::{MacosServer, MailSearchParams, MailSyncParams};
use rmcp::handler::server::wrapper::Parameters;

fn server() -> MacosServer {
    let dir = tempfile::tempdir().unwrap();
    std::mem::forget(dir);
    MacosServer::new(std::path::PathBuf::from("/nonexistent-state"))
}

#[tokio::test]
async fn mail_search_rejects_unknown_source() {
    let s = server();
    let p: MailSearchParams = serde_json::from_value(serde_json::json!({
        "query": "x", "source": "banana"
    }))
    .unwrap();
    let out = s.mail_search(Parameters(p)).await;
    assert!(out.contains("source"), "{out}");
    assert!(out.contains("error"), "{out}");
}

#[tokio::test]
async fn mail_search_index_mode_reports_missing_index_without_transport() {
    // No sync has ever run → index.db does not exist. Index mode must fail
    // with an actionable hint, NOT spawn osascript.
    let s = server();
    let p: MailSearchParams = serde_json::from_value(serde_json::json!({
        "query": "assessment", "source": "index", "folders": ["Exchange/Apps"]
    }))
    .unwrap();
    let out = s.mail_search(Parameters(p)).await;
    assert!(out.contains("mail_sync"), "{out}");
}

#[tokio::test]
async fn mail_sync_star_rejected_in_open_scope_with_hint() {
    let s = server(); // default scope is open
    let p: MailSyncParams = serde_json::from_value(serde_json::json!({
        "folders": ["*"]
    }))
    .unwrap();
    let out = s.mail_sync(Parameters(p)).await;
    assert!(out.contains("scope"), "{out}");
    assert!(out.contains("error"), "{out}");
}

#[tokio::test]
async fn mail_sync_respects_disabled_group() {
    let dir = tempfile::tempdir().unwrap();
    let s = MacosServer::new_with_tools(
        dir.path().to_path_buf(),
        mcp_macos::EnabledTools::all(),
        mcp_macos::policy::MailPolicy::default(),
        mcp_macos::policy::EffectiveScope::open(),
    );
    // EnabledTools::all includes mail; the disabled branch needs a custom
    // set without it — construct via parse if available.
    drop(s);
    let disabled = mcp_macos::EnabledTools::parse("clipboard,messages").unwrap();
    let s2 = MacosServer::new_with_tools(
        dir.path().to_path_buf(),
        disabled,
        mcp_macos::policy::MailPolicy::default(),
        mcp_macos::policy::EffectiveScope::open(),
    );
    let p: MailSyncParams = serde_json::from_value(serde_json::json!({})).unwrap();
    let out = s2.mail_sync(Parameters(p)).await;
    assert!(out.contains("disabled"), "{out}");
}
