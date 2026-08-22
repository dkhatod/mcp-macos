//! Notifications toolset contract tests (mock transport).

use mcp_macos::notifications::NotificationsToolset;
use personai_core::macos::MockTransport;

#[tokio::test]
async fn post_script_contains_payload() {
    let mut t = MockTransport::new();
    t.enqueue(r#"{"ok":true,"value":{"status":"posted"}}"#);
    let mut ts = NotificationsToolset::new(t);
    let res = ts.post("personai", "Check complete", None).await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&res).unwrap();
    assert_eq!(v["status"], "posted");
    let script = &ts.transport.calls()[0].script;
    assert!(
        script.contains(r#""Check complete""#) && script.contains(r#""personai""#),
        "payload missing from script: {script}"
    );
}

#[tokio::test]
async fn post_escapes_quotes_in_payload() {
    let mut t = MockTransport::new();
    t.enqueue(r#"{"ok":true,"value":{"status":"posted"}}"#);
    let mut ts = NotificationsToolset::new(t);
    ts.post("say \"hi\"", r"line\nbreak", Some("sub\"ject"))
        .await
        .unwrap();
    let script = &ts.transport.calls()[0].script;
    assert!(script.contains(r#"say \"hi\""#), "{script}");
    assert!(script.contains(r#"sub\"ject"#), "{script}");
}

#[tokio::test]
async fn response_shape_has_status_and_title() {
    let mut t = MockTransport::new();
    t.enqueue(r#"{"ok":true,"value":{"status":"posted"}}"#);
    let mut ts = NotificationsToolset::new(t);
    let res = ts.post("t", "m", None).await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&res).unwrap();
    assert_eq!(v["status"], "posted");
    assert_eq!(v["title"], "t");
}
