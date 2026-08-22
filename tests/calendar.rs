//! Calendar toolset contract tests (mock transport).

use mcp_macos::calendar::CalendarToolset;
use personai_core::macos::MockTransport;
use personai_core::safety::SoftGate;

struct Fixture {
    _dir: tempfile::TempDir,
    ts: CalendarToolset<MockTransport>,
}

fn fixture(envelopes: &[&str]) -> Fixture {
    let mut t = MockTransport::new();
    for e in envelopes {
        t.enqueue(*e);
    }
    let dir = tempfile::tempdir().unwrap();
    let mut ts = CalendarToolset::new(t);
    ts.gate = Some(SoftGate::new(dir.path().join("tokens.json")).unwrap());
    Fixture { _dir: dir, ts }
}

#[tokio::test]
async fn list_returns_calendar_names() {
    let mut f = fixture(&[r#"{"ok":true,"value":["Home","Work"]}"#]);
    let res = f.ts.list().await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&res).unwrap();
    assert_eq!(v["calendars"], serde_json::json!(["Home", "Work"]));
}

#[tokio::test]
async fn read_is_paginated_with_total() {
    let mut f = fixture(&[
        r#"{"ok":true,"value":{"total":2,"offset":0,"limit":10,"events":[{"id":"e1","title":"Acme phone screen","start":"2026-08-21T14:00:00Z","end":"2026-08-21T14:30:00Z"},{"id":"e2","title":"Lunch","start":"2026-08-21T12:00:00Z","end":"2026-08-21T13:00:00Z"}]}}"#,
    ]);
    let res =
        f.ts.read(
            Some("2026-08-21T00:00:00Z".into()),
            Some("2026-08-22T00:00:00Z".into()),
            Some(10),
            0,
        )
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&res).unwrap();
    assert_eq!(v["total"], 2);
    assert_eq!(v["events"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn read_escapes_dates_into_script() {
    let mut f = fixture(&[r#"{"ok":true,"value":{"total":0,"events":[]}}"#]);
    let _ =
        f.ts.read(
            Some("2026-08-21T00:00:00Z".into()),
            Some("2026-08-22T00:00:00Z".into()),
            None,
            0,
        )
        .await
        .unwrap();
    let script = &f.ts.transport.calls()[0].script;
    assert!(
        script.contains("Date.parse(\"2026-08-21T00:00:00Z\")"),
        "{script}"
    );
}

#[tokio::test]
async fn create_is_soft_gated_then_executes() {
    let mut f = fixture(&[r#"{"ok":true,"value":{"id":"e9"}}"#]);
    let first =
        f.ts.create(
            "Dentist",
            "2026-08-25T10:00:00Z",
            "2026-08-25T10:30:00Z",
            None,
        )
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert_eq!(v["status"], "requires_confirmation");
    assert_eq!(f.ts.transport.calls().len(), 0);

    let token = v["confirmation_token"].as_str().unwrap().to_string();
    let second =
        f.ts.create(
            "Dentist",
            "2026-08-25T10:00:00Z",
            "2026-08-25T10:30:00Z",
            Some(&token),
        )
        .await
        .unwrap();
    let v2: serde_json::Value = serde_json::from_str(&second).unwrap();
    assert_eq!(v2["status"], "created");
    assert_eq!(v2["id"], "e9");
}

#[tokio::test]
async fn update_is_soft_gated() {
    let mut f = fixture(&[r#"{"ok":true,"value":{"id":"e1"}}"#]);
    let first =
        f.ts.update("e1", Some("New title"), None, None, None)
            .await
            .unwrap();
    let v: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert_eq!(v["status"], "requires_confirmation");

    let token = v["confirmation_token"].as_str().unwrap().to_string();
    let second =
        f.ts.update("e1", Some("New title"), None, None, Some(&token))
            .await
            .unwrap();
    let v2: serde_json::Value = serde_json::from_str(&second).unwrap();
    assert_eq!(v2["status"], "updated");
}

#[cfg(target_os = "macos")]
mod real {
    use super::*;
    use personai_core::macos::JxaTransport;

    /// Real Calendar round trip — strictly read-only. Skips on CI runners
    /// where the TCC consent prompt cannot be approved (timeout/permission).
    #[tokio::test]
    async fn real_calendar_list_and_read() {
        let mut ts = CalendarToolset::new(JxaTransport);
        let lists = match ts.list().await {
            Ok(l) => l,
            Err(e)
                if e.to_string().contains("permission denied")
                    || e.to_string().contains("timed out") =>
            {
                eprintln!("skipping: no Calendar automation grant on this host ({e})");
                return;
            }
            Err(e) => panic!("list: {e}"),
        };
        assert!(lists.contains("calendars"));
        let res = ts
            .read(
                Some("2026-07-01T00:00:00Z".into()),
                Some("2026-09-30T00:00:00Z".into()),
                Some(5),
                0,
            )
            .await
            .unwrap_or_else(|e| panic!("read: {e}"));
        let v: serde_json::Value = serde_json::from_str(&res).unwrap();
        assert!(v["events"].as_array().is_some());
    }
}
