//! LIVE structural smoke for the mail index — run MANUALLY on a Mac with
//! Mail configured:
//!
//! ```sh
//! cargo test --test mail_live_smoke -- --ignored --nocapture
//! ```
//!
//! Why this exists: MockTransport models the *transport*, not *Mail.app
//! semantics*. The special `Google/Notes` / `Exchange/Notes` mailboxes throw
//! AppleEvent -1728 on bulk message fetches, and every mock said "success" —
//! so the whole suite was green while `mail_sync` died on the first poison
//! folder in production. This test walks EVERY account/mailbox pair on the
//! machine and proves the fetch pattern either succeeds or degrades to a
//! recorded per-folder error.
//!
//! Privacy: folder NAMES and message COUNTS only. Message subjects, senders,
//! bodies are never requested (`scan=1` fetches one `id()` — an integer —
//! purely to prove the bulk path executes).

use mcp_macos::mail_index::sync_expr;
use personai_core::macos::{AppleTransport, JxaTransport};

// `#[ignore]`: needs a Mac with Mail configured + Automation permission.
// Run explicitly: cargo test --test mail_live_smoke -- --ignored --nocapture
#[tokio::test]
#[ignore = "live Mail.app required; run with --ignored"]
async fn real_mail_every_folder_survives_the_bulk_fetch_pattern() {
    let mut t = JxaTransport;

    // 1. Enumerate accounts and their mailbox names + counts.
    let map_script = r#"(() => {
  const M = Application('Mail');
  const out = [];
  for (const a of M.accounts()) {
    for (const mb of a.mailboxes()) {
      let c = null;
      try { c = mb.messages.length; } catch (e) { c = -1; }
      out.push({account: a.name(), mailbox: mb.name(), count: c});
    }
  }
  return out;
})()"#;

    let raw = t
        .run(
            &personai_core::macos::wrap_jxa(map_script),
            std::time::Duration::from_secs(30),
        )
        .await
        .expect("Mail.app must be running for this smoke test");
    let parsed: Value = parse_envelope(&raw);
    let boxes = parsed.as_array().cloned().unwrap_or_default();
    assert!(
        !boxes.is_empty(),
        "no mailboxes found — is Mail configured?"
    );

    // 2. Run the EXACT sync fetch pattern against every pair.
    let mut degraded: Vec<String> = Vec::new();
    let mut checked = 0usize;
    for b in &boxes {
        let account = b["account"].as_str().unwrap_or_default();
        let mailbox = b["mailbox"].as_str().unwrap_or_default();
        if mailbox.is_empty() {
            continue;
        }
        checked += 1;
        let expr = sync_expr(account, mailbox, None, 1);
        let raw = t
            .run(
                &personai_core::macos::wrap_jxa(&expr),
                std::time::Duration::from_secs(30),
            )
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "transport-level failure on {account}/{mailbox}: {e} — an unguarded \
                        code path escaped the JS try/catch"
                )
            });
        let v = parse_envelope(&raw);
        if let Some(err) = v.get("error").and_then(Value::as_str) {
            degraded.push(format!("{account}/{mailbox}: {err}"));
        }
    }

    println!("checked {checked} mailboxes; degraded (recorded per-folder): {degraded:?}");
    // Degrading is FINE — that is the isolation working. What must NEVER
    // happen again is a transport-level escape (the panic above).
    assert!(checked > 0);
}

use serde_json::Value;

/// Minimal local envelope parser (mirrors personai-core's `parse_envelope`
/// without pulling the error taxonomy into assertions we don't need here).
fn parse_envelope(raw: &str) -> Value {
    let v: Value = serde_json::from_str(raw.trim())
        .unwrap_or_else(|e| panic!("osascript output was not a JSON envelope: {e}: {raw}"));
    let ok = v.get("ok").and_then(Value::as_bool).unwrap_or(false);
    assert!(ok, "AppleEvent error surfaced: {v}");
    v["value"].clone()
}
