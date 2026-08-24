//! Contract test: narrowed searches must fail with GUIDANCE when the caller
//! passes an unknown account name (e.g. an email address instead of the
//! display name Mail.app shows). A live session once burned turns on
//! `AppleEvent -1728: Can't get object` doing exactly that.

use mcp_macos::mail::MailToolset;
use personai_core::macos::MockTransport;
use serde_json::{Value, json};

fn envelope(v: Value) -> String {
    json!({"ok": true, "value": v}).to_string()
}

#[tokio::test]
async fn unknown_account_returns_available_accounts_instead_of_minus_1728() {
    let mut ts = MailToolset::new(MockTransport::new());
    // The pre-flight name check consumes this envelope.
    ts.transport
        .enqueue(&envelope(json!(["Exchange", "Google", "iCloud"])));
    let out = ts
        .search(
            "application",
            &[],
            Some("dhruvkhatod@gmail.com"), // email ≠ display name
            None,
            None,
            None,
            None,
            0,
            5000,
            false,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .contains("dhruvkhatod@gmail.com")
    );
    assert_eq!(
        v["available_accounts"],
        json!(["Exchange", "Google", "iCloud"]),
        "agent must be able to self-correct in one turn"
    );
    assert_eq!(
        ts.transport.calls().len(),
        1,
        "no search script may reach osascript after a failed name check"
    );
}

#[tokio::test]
async fn valid_account_still_executes_the_search() {
    let mut ts = MailToolset::new(MockTransport::new());
    ts.transport
        .enqueue(&envelope(json!(["Exchange", "Google", "iCloud"])));
    ts.transport.enqueue(&envelope(json!({
        "total": 0, "results": []
    })));
    let out = ts
        .search(
            "x",
            &[],
            Some("Google"),
            None,
            None,
            None,
            None,
            0,
            5000,
            false,
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["total"], 0);
    assert_eq!(ts.transport.calls().len(), 2, "name check + search");
}

#[tokio::test]
async fn mail_config_reports_its_own_version() {
    // Live-run debugging requires knowing WHICH server binary answered.
    let s = mcp_macos::MacosServer::new(std::env::temp_dir().join("cfg-ver-test"));
    let out = s.mail_config().await;
    assert!(
        out.contains(env!("CARGO_PKG_VERSION")),
        "mail_config must surface the server version: {out}"
    );
}
