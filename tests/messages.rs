//! Messages toolset contract tests (mock transport).

use mcp_macos::messages::MessagesToolset;
use personai_core::macos::MockTransport;
use personai_core::safety::SoftGate;

struct Fixture {
    _dir: tempfile::TempDir,
    ts: MessagesToolset<MockTransport>,
}

fn fixture(envelopes: &[&str]) -> Fixture {
    let mut t = MockTransport::new();
    for e in envelopes {
        t.enqueue(e);
    }
    let dir = tempfile::tempdir().unwrap();
    let mut ts = MessagesToolset::new(t);
    ts.gate = Some(SoftGate::new(dir.path().join("tokens.json")).unwrap());
    Fixture { _dir: dir, ts }
}

#[tokio::test]
async fn read_is_paginated_metadata() {
    let mut f = fixture(&[
        r#"{"ok":true,"value":{"total":5,"offset":0,"limit":10,"messages":[{"chat":"Mom","direction":"in","text":"Call me","date":"2026-08-19T12:00:00Z"}]}}"#,
    ]);
    let res = f.ts.read(Some("Mom".into()), Some(10), 0).await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&res).unwrap();
    assert_eq!(v["total"], 5);
    assert_eq!(v["offset"], 0);
    assert!(!v["messages"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn read_escapes_chat_filter_into_script() {
    let mut f = fixture(&[r#"{"ok":true,"value":{"total":0,"messages":[]}}"#]);
    let _ = f.ts.read(Some("O'Brien".into()), None, 0).await.unwrap();
    let script = &f.ts.transport.calls()[0].script;
    // The chat filter lands in SQL, so single quotes must be doubled.
    assert!(script.contains("'O''Brien'"), "chat not escaped: {script}");
}

#[tokio::test]
async fn send_is_soft_gated_then_executes() {
    let mut f = fixture(&[r#"{"ok":true,"value":{"status":"sent"}}"#]);
    let first = f.ts.send("+15550001111", "Hi!", None).await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert_eq!(v["status"], "requires_confirmation");
    assert_eq!(v["payload"]["to"], "+15550001111");
    assert_eq!(
        f.ts.transport.calls().len(),
        0,
        "no transport call before confirm"
    );

    let token = v["confirmation_token"].as_str().unwrap().to_string();
    let second =
        f.ts.send("+15550001111", "Hi!", Some(&token))
            .await
            .unwrap();
    let v2: serde_json::Value = serde_json::from_str(&second).unwrap();
    assert_eq!(v2["status"], "sent");
    assert_eq!(f.ts.transport.calls().len(), 1);
}

#[tokio::test]
async fn send_without_gate_refuses() {
    let t = MockTransport::new();
    let mut ts = MessagesToolset::new(t);
    ts.gate = None;
    let err = ts.send("+15550001111", "Hi!", None).await.unwrap_err();
    assert!(err.to_string().contains("gate"));
}

/// macOS only, read-only: reads real chat.db. Requires Full Disk Access for
/// the calling terminal; skips (does not fail) when the DB is unreadable so
/// CI runners without FDA still pass.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn real_messages_read() {
    use personai_core::macos::JxaTransport;
    let mut ts = MessagesToolset::new(JxaTransport);
    match ts.read(None, Some(3), 0).await {
        Ok(res) => {
            let v: serde_json::Value = serde_json::from_str(&res).unwrap();
            assert!(v["messages"].as_array().is_some());
        }
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("Permission denied") || msg.contains("unable to open database"),
                "unexpected messages_read failure: {msg}"
            );
        }
    }
}
