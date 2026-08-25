//! Contract: soft-gated sends respect an OPTIONAL recipient allowlist at
//! `{state_dir}/messages-send-allowlist.json` (JSON array of handles).
//! Missing/empty file = allow all (back-compat). Normalization: emails
//! lowercase; phones compare digits-only, so formatting cannot smuggle a
//! different number through.

use mcp_macos::{MacosServer, MessagesSendParams};
use rmcp::handler::server::wrapper::Parameters;

fn server_with_allowlist(content: Option<&str>) -> MacosServer {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state");
    // The gate's token store needs its parent dir to exist (production
    // creates state_dir before wiring the server).
    std::fs::create_dir_all(&path).unwrap();
    if let Some(c) = content {
        std::fs::write(path.join("messages-send-allowlist.json"), c).unwrap();
    }
    std::mem::forget(dir);
    MacosServer::new(path)
}

fn params(to: &str) -> Parameters<MessagesSendParams> {
    Parameters(
        serde_json::from_value(serde_json::json!({ "to": to, "body": "hi" })).unwrap(),
    )
}

#[tokio::test]
async fn allowlist_blocks_unlisted_recipient_before_gate() {
    let s = server_with_allowlist(Some(r#"["+17038140603"]"#));
    let out = s.messages_send(params("evil@attacker.com")).await;
    assert!(out.contains("not in the send allowlist"), "{out}");
    assert!(out.contains("available"), "{out}");
}

#[tokio::test]
async fn allowlist_accepts_formatted_equivalent_phone() {
    let s = server_with_allowlist(Some(r#"["7038140603"]"#));
    // Listed without country code; caller uses E164 — digits must match.
    let out = s.messages_send(params("+17038140603")).await;
    assert!(
        !out.contains("not in the send allowlist"),
        "formatted equivalent must pass: {out}"
    );
}

#[tokio::test]
async fn missing_allowlist_file_preserves_open_behavior() {
    let s = server_with_allowlist(None);
    let out = s.messages_send(params("anyone@x.com")).await;
    assert!(
        out.contains("requires_confirmation") || out.contains("soft gate"),
        "no allowlist → straight to the normal gate flow: {out}"
    );
}
