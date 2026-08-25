//! Messages toolset contract tests (mock transport).

mod common;

use common::balanced;
use mcp_macos::messages::MessagesToolset;
use personai_core::macos::MockTransport;
use personai_core::safety::SoftGate;
use serde_json::{Value, json};

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
    // Privacy-first: live reads only run when a chat is explicitly scoped
    // via MCP_MESSAGES_LIVE_CHAT (see tests/messages_live_scoped.rs).
    let Some(chat) = std::env::var("MCP_MESSAGES_LIVE_CHAT").ok() else {
        eprintln!("skipped: set MCP_MESSAGES_LIVE_CHAT=<identifier> to opt in");
        return;
    };
    use personai_core::macos::JxaTransport;
    let mut ts = MessagesToolset::new(JxaTransport);
    match ts.read(Some(chat), Some(3), 0).await {
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

// --- Hostile content: the envelope must survive ANY stored text ------------

#[tokio::test]
async fn read_tolerates_control_characters_in_text() {
    // Live finding: real message text can carry characters SQL's
    // REPLACE(char(13)/(10)) does not strip (vertical tab, U+2028…). The
    // mapper builds single-line JSON, so one stray control char corrupted
    // the whole envelope ("trailing characters at line 3").
    // Payload travels as a PRESERIALIZED string (house contract); the
    // hostile character rides INSIDE it, exactly like live sqlite output.
    // U+2028/U+2029 pass serde_json parsing untouched (valid JSON!) yet break
    // naive single-line consumers — exactly the live corruption class. Raw
    // C0 controls (<0x20) cannot reach this layer: both parse stages reject
    // them first, so they are not part of the reachable threat model here.
    let hostile = "line\u{2028}sep\u{2029}end";
    let payload = format!(
        "{{\"total\":1,\"messages\":[\n{{\"from\":\"a@x\",\"direction\":\"in\",\"text\":\"{hostile}\",\"date\":\"2026-08-24T00:00:00Z\"}}\n]}}"
    );
    let canned = json!({ "ok": true, "value": payload }).to_string();
    let mut f = fixture(&[canned.as_str()]);
    let out = f.ts.read(Some("a@x".into()), Some(5), 0).await.unwrap();
    // The returned payload must re-parse cleanly — no raw control chars.
    let v: Value = serde_json::from_str(&out)
        .unwrap_or_else(|e| panic!("envelope corrupted by stored text: {e}: {out}"));
    assert_eq!(v["total"], 1);
}

#[tokio::test]
async fn chats_tolerate_control_characters_in_display_names() {
    let mut f = fixture(&[r#"{"ok":true,"value":{"total":1,"chats":[
        {"identifier":"chat-1","display_name":"bad\u2028name","service":"iMessage","handle":"a@x","message_count":2,"last_activity":"2026-08-24"}
    ]}}"#]);
    let out = f.ts.chats(Some(5), 0).await.unwrap();
    assert!(out.contains("bad"), "{out}");
}

// --- Group-chat resolution -------------------------------------------------

#[tokio::test]
async fn chats_surface_group_flag_and_participants() {
    // Inner payload built programmatically — hand-escaping two JSON levels
    // is how typos become "mystery" parse failures.
    let payload = serde_json::json!({
        "total": 2,
        "chats": [
            {"identifier":"chat-9","display_name":"Weekend Crew","service":"iMessage",
             "handle":"a@x","message_count":42,"last_activity":"2026-08-24T01:00:00Z",
             "participant_count":4,"participants":["a@x","b@x","c@x","d@x"],
             "is_group": true},
            {"identifier":"+15551234567","display_name":"","service":"iMessage",
             "handle":"+15551234567","message_count":7,"last_activity":"2026-08-23T01:00:00Z",
             "participant_count":1,"participants":["+15551234567"],
             "is_group": false}
        ]
    });
    let envelope = serde_json::json!({"ok": true, "value": payload.to_string()});
    let canned = envelope.to_string();
    let mut f = fixture(&[canned.as_str()]);
    let out = f.ts.chats(Some(5), 0).await.unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    let chats = v["chats"].as_array().unwrap();
    assert_eq!(chats[0]["is_group"], true);
    assert_eq!(chats[0]["participants"].as_array().unwrap().len(), 4);
    assert_eq!(chats[0]["participant_count"], 4);
    assert_eq!(chats[1]["is_group"], false, "1:1 chat by participant count");
}

// --- Unread digest + attachment listing -------------------------------------

#[test]
fn unread_builder_filters_is_read_and_groups_by_chat() {
    let script = mcp_macos::messages::unread_expr_for_test(20);
    assert!(script.contains("m.is_from_me=0"), "{script}");
    assert!(script.contains("m.is_read=0"), "{script}");
    assert!(script.contains("GROUP BY c.ROWID"), "{script}");
}

#[tokio::test]
async fn unread_surfaces_counts_newest_first() {
    let payload = serde_json::json!({
        "total": 2,
        "chats": [
            {"chat":"chat-9","display_name":"Weekend Crew","unread":5,"last_activity":"2026-08-24T01:00:00Z"},
            {"chat":"+15551234567","display_name":"","unread":1,"last_activity":"2026-08-23T09:00:00Z"}
        ]
    });
    let envelope = serde_json::json!({"ok": true, "value": payload.to_string()});
    let canned = envelope.to_string();
    let mut f = fixture(&[canned.as_str()]);
    let out = f.ts.unread(Some(10)).await.unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["total"], 2);
    assert_eq!(v["chats"][0]["unread"], 5);
}

#[test]
fn attachments_builder_escapes_chat_and_lists_metadata_only() {
    let script = mcp_macos::messages::attachments_expr_for_test(Some("O'Brien"), 20, 0);
    assert!(
        script.contains("O''Brien"),
        "chat must be SQL-escaped: {script}"
    );
    assert!(script.contains("transfer_name"), "{script}");
    assert!(
        !script.contains("data IS NOT NULL"),
        "never selects blob content"
    );
}

#[tokio::test]
async fn attachments_surface_names_sizes_without_content() {
    let payload = serde_json::json!({
        "total": 1, "offset": 0, "limit": 20,
        "attachments": [
            {"name":"offer.pdf","mime":"application/pdf","bytes":182044,"date":"2026-08-01T12:00:00Z","chat":"a@x"}
        ]
    });
    let envelope = serde_json::json!({"ok": true, "value": payload.to_string()});
    let canned = envelope.to_string();
    let mut f = fixture(&[canned.as_str()]);
    let out =
        f.ts.attachments(Some("a@x".into()), Some(5), 0)
            .await
            .unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["total"], 1);
    assert_eq!(v["attachments"][0]["name"], "offer.pdf");
    assert_eq!(v["attachments"][0]["bytes"], 182044);
}
