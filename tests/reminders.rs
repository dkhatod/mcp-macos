//! Reminders toolset contract tests. All run on every OS via `MockTransport`.

use mcp_macos::reminders::RemindersToolset;
use personai_core::macos::MockTransport;
use serde_json::Value;

struct Fixture {
    _dir: tempfile::TempDir,
    ts: RemindersToolset<MockTransport>,
}

fn fixture(envelopes: &[&str]) -> Fixture {
    let mut t = MockTransport::new();
    for e in envelopes {
        t.enqueue(e);
    }
    let dir = tempfile::tempdir().unwrap();
    let ts = RemindersToolset::with_gate(t, dir.path().join("tokens.json")).unwrap();
    Fixture { _dir: dir, ts }
}

fn parse(s: &str) -> Value {
    serde_json::from_str(s).unwrap()
}

#[tokio::test]
async fn list_lists_passes_through() {
    let env = r#"{"ok":true,"value":{"lists":[{"name":"Personal"},{"name":"Work"}]}}"#;
    let mut f = fixture(&[env]);
    let v = parse(&f.ts.list_lists().await.unwrap());
    assert_eq!(v["lists"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn read_targets_named_list_and_filters_open_items() {
    let env = r#"{"ok":true,"value":{"total":1,"offset":3,"limit":20,"reminders":[{"id":"r1","name":"Call bank","due":null,"body":"","priority":0,"completed":false}]}}"#;
    let mut f = fixture(&[env]);
    let v = parse(
        &f.ts
            .read(Some("Work".into()), false, None, 3)
            .await
            .unwrap(),
    );
    assert_eq!(v["total"], 1);
    assert_eq!(v["offset"], 3);
    let script = &f.ts.transport.calls()[0].script;
    assert!(
        script.contains(r#"whose({name: "Work"})"#),
        "named list targeted: {script}"
    );
    assert!(script.contains("list not found"), "{script}");
    assert!(
        script.contains("!wantDone && done && done[i]"),
        "open-items filter present: {script}"
    );
}

#[tokio::test]
async fn create_is_soft_gated_then_executes_with_fields() {
    let mut f = fixture(&[
        r#"{"ok":true,"value":{"status":"created","id":"r9"}}"#,
        r#"{"ok":true,"value":{"status":"created","id":"r9"}}"#,
    ]);
    let first =
        f.ts.create(
            "Follow up with GS",
            Some("Work"),
            Some("2026-08-25T09:00:00Z"),
            Some("assessment due"),
            None,
        )
        .await
        .unwrap();
    let v = parse(&first);
    assert_eq!(v["status"], "requires_confirmation");
    assert_eq!(f.ts.transport.calls().len(), 0);

    let token = v["confirmation_token"].as_str().unwrap().to_string();
    let second =
        f.ts.create(
            "Follow up with GS",
            Some("Work"),
            Some("2026-08-25T09:00:00Z"),
            Some("assessment due"),
            Some(&token),
        )
        .await
        .unwrap();
    assert_eq!(parse(&second)["status"], "created");

    let script = &f.ts.transport.calls()[0].script;
    assert!(script.contains(r#"whose({name: "Work"})"#), "{script}");
    assert!(
        script.contains(r#"new Date("2026-08-25T09:00:00Z")"#),
        "{script}"
    );
    assert!(script.contains(r#"r.body = "assessment due";"#), "{script}");
}

#[tokio::test]
async fn complete_is_soft_gated_and_uses_whose_id() {
    let mut f = fixture(&[
        r#"{"ok":true,"value":{"status":"completed","id":"r1"}}"#,
        r#"{"ok":true,"value":{"status":"completed","id":"r1"}}"#,
    ]);
    let v = parse(&f.ts.complete("r1", None).await.unwrap());
    assert_eq!(v["status"], "requires_confirmation");

    let token = v["confirmation_token"].as_str().unwrap().to_string();
    let v2 = parse(&f.ts.complete("r1", Some(&token)).await.unwrap());
    assert_eq!(v2["status"], "completed");
    let script = &f.ts.transport.calls()[0].script;
    assert!(script.contains(r#"whose({id: "r1"})"#), "{script}");
}
