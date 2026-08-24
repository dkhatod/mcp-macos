//! Messages toolset contract tests (mock transport).

mod common;

use common::balanced;
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

#[tokio::test]
async fn chats_lists_discovery_with_pagination() {
    let mut f = fixture(&[
        r#"{"ok":true,"value":{"total":2,"offset":0,"limit":20,"chats":[{"identifier":"+1555","display_name":"Mom","service":"iMessage","handle":"+1555","message_count":42,"last_activity":"2026-08-23 12:00:00"}]}}"#,
    ]);
    let res = f.ts.chats(Some(20), 0).await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&res).unwrap();
    assert_eq!(v["total"], 2);
    assert_eq!(v["chats"][0]["display_name"], "Mom");
    let script = &f.ts.transport.calls()[0].script;
    assert!(
        script.contains("SELECT COUNT(*) FROM chat"),
        "count and page must share one invocation: {script}"
    );
    assert!(script.contains("GROUP BY c.ROWID"), "{script}");
}

#[tokio::test]
async fn read_emits_single_invocation_with_total_header() {
    let mut f =
        fixture(&[r#"{"ok":true,"value":{"total":377160,"offset":0,"limit":30,"messages":[]}}"#]);
    let v: serde_json::Value =
        serde_json::from_str(&f.ts.read(None, Some(30), 0).await.unwrap()).unwrap();
    assert_eq!(v["total"], 377160);
    let script = &f.ts.transport.calls()[0].script;
    // ONE sqlite3 call: total header line + page, consistent by construction.
    let count = script.matches("sqlite3").count();
    assert_eq!(count, 1, "single invocation expected: {script}");
    assert!(script.contains("SELECT COUNT(*)"), "{script}");
    assert!(script.contains("LIMIT 30 OFFSET 0"), "{script}");
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

#[tokio::test]
async fn read_unwraps_preserialized_row_per_line_payload() {
    // Real path: the JXA builder returns its payload as a STRING (one row
    // per line); the envelope carries it as Value::String and the toolset
    // must re-parse so callers get an object, never a quoted JSON blob.
    let inner = "{\"total\":2,\"messages\":[\n{\"from\":\"+1555\",\"direction\":\"in\",\"text\":\"hi\",\"date\":\"2026-08-23T12:00:00Z\"},\n{\"from\":\"me\",\"direction\":\"out\",\"text\":\"yo\",\"date\":\"2026-08-23T12:01:00Z\"}\n]}";
    let env = format!(
        r#"{{"ok":true,"value":{}}}"#,
        serde_json::to_string(inner).unwrap()
    );
    let mut f = fixture(&[&env]);
    let v: serde_json::Value =
        serde_json::from_str(&f.ts.read(None, Some(3), 0).await.unwrap()).unwrap();
    assert_eq!(v["total"], 2);
    assert_eq!(v["messages"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn chats_unwraps_preserialized_payload() {
    let inner = "{\"total\":1,\"chats\":[\n{\"identifier\":\"+1555\",\"display_name\":\"Mom\",\"service\":\"iMessage\",\"handle\":\"+1555\",\"message_count\":9,\"last_activity\":\"2026-08-23T09:00:00Z\"}\n]}";
    let env = format!(
        r#"{{"ok":true,"value":{}}}"#,
        serde_json::to_string(inner).unwrap()
    );
    let mut f = fixture(&[&env]);
    let v: serde_json::Value =
        serde_json::from_str(&f.ts.chats(Some(20), 0).await.unwrap()).unwrap();
    assert_eq!(v["chats"][0]["display_name"], "Mom");
}

/// Balance oracle over every generated Messages script.
#[tokio::test]
async fn generated_scripts_are_syntactically_balanced() {
    // read_expr
    let mut f = fixture(&[r#"{"ok":true,"value":{"total":0,"messages":[]}}"#]);
    let _ = f.ts.read(None, None, 0).await.unwrap();
    assert!(
        balanced(&f.ts.transport.calls()[0].script),
        "read script unbalanced"
    );

    // chats_expr
    let mut g = fixture(&[r#"{"ok":true,"value":{"total":0,"chats":[]}}"#]);
    let _ = g.ts.chats(None, 0).await.unwrap();
    assert!(
        balanced(&g.ts.transport.calls()[0].script),
        "chats script unbalanced"
    );

    // send_expr (execute phase of the soft gate)
    let mut h = fixture(&[r#"{"ok":true,"value":{"status":"sent"}}"#]);
    let first = h.ts.send("+15550001111", "Hi!", None).await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&first).unwrap();
    let token = v["confirmation_token"].as_str().unwrap().to_string();
    let _ =
        h.ts.send("+15550001111", "Hi!", Some(&token))
            .await
            .unwrap();
    assert!(
        balanced(&h.ts.transport.calls()[0].script),
        "send script unbalanced"
    );
}

/// MCP_MACOS_DEBUG_RAW diagnostic: with the env var set, an unparseable
/// envelope triggers a second transport call that captures the raw stdout;
/// without it, zero extra calls. The Parse error propagates either way.
#[tokio::test]
async fn debug_raw_recaptures_stdout_only_when_env_gated() {
    // Deliberately multi-line stdout — mimics the live quirk where
    // osascript prints extra lines around the JSON envelope.
    let broken = "{\"ok\":true,\"value\":{\"total\":0}}\nstray line\n";

    // Env unset (the production default): single call, no recapture.
    unsafe {
        std::env::remove_var("MCP_MACOS_DEBUG_RAW");
    }
    let mut t = MockTransport::new();
    t.enqueue(broken);
    let mut ts = MessagesToolset::new(t);
    let err = ts.read(None, None, 0).await.unwrap_err();
    assert!(err.to_string().contains("trailing"), "{err}");
    assert_eq!(ts.transport.calls().len(), 1, "no extra call when unset");

    // Env set: same failure, but the raw stdout is re-captured.
    unsafe {
        std::env::set_var("MCP_MACOS_DEBUG_RAW", "1");
    }
    let mut t = MockTransport::new();
    t.enqueue(broken);
    t.enqueue(r#"{"ok":true,"value":{"total":0}}"#); // consumed by the recapture
    let mut ts = MessagesToolset::new(t);
    let err = ts.read(None, None, 0).await.unwrap_err();
    assert!(
        err.to_string().contains("trailing"),
        "error must still propagate"
    );
    assert_eq!(ts.transport.calls().len(), 2, "diagnostic re-run expected");
    assert!(
        ts.transport.calls()[1]
            .script
            .contains("JSON.stringify({ok"),
        "recapture must use the wrap_jxa envelope"
    );
    unsafe {
        std::env::remove_var("MCP_MACOS_DEBUG_RAW");
    }
}
