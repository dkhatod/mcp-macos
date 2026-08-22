//! Mail toolset contract tests. All run on every OS via `MockTransport`.

use mcp_macos::mail::MailToolset;
use personai_core::macos::MockTransport;
use personai_core::safety::SoftGate;
use serde_json::json;

/// A toolset over a mock transport plus its temp dir (for the token store).
struct Fixture {
    _dir: tempfile::TempDir,
    ts: MailToolset<MockTransport>,
}

fn fixture(envelopes: &[&str]) -> Fixture {
    let mut t = MockTransport::new();
    for e in envelopes {
        t.enqueue(*e);
    }
    let dir = tempfile::tempdir().unwrap();
    let mut ts = MailToolset::new(t);
    ts.gate = Some(SoftGate::new(dir.path().join("tokens.json")).unwrap());
    Fixture { _dir: dir, ts }
}

#[tokio::test]
async fn list_accounts_returns_names() {
    let mut f = fixture(&[r#"{"ok":true,"value":["Google","Exchange"]}"#]);
    let res = f.ts.list_accounts().await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&res).unwrap();
    assert_eq!(v["accounts"], json!(["Google", "Exchange"]));
}

#[tokio::test]
async fn search_returns_metadata_only_and_paginates() {
    // Transport returns 3 full messages (with bodies!) — the toolset must
    // cap the page, drop bodies, and keep the true total visible.
    let mailbox = json!({
        "total": 3,
        "results": [
            {"id": "m1", "subject": "s1", "from": "a@x", "date": "2026-08-19T10:00:00Z", "snippet": "sn1", "body": "BODY1"},
            {"id": "m2", "subject": "s2", "from": "b@x", "date": "2026-08-19T09:00:00Z", "snippet": "sn2", "body": "BODY2"},
            {"id": "m3", "subject": "s3", "from": "c@x", "date": "2026-08-18T09:00:00Z", "snippet": "sn3", "body": "BODY3"}
        ]
    });
    let env = format!(r#"{{"ok":true,"value":{mailbox}}}"#);
    let mut f = fixture(&[&env]);
    let res =
        f.ts.search("application", None, None, Some(2), 0)
            .await
            .unwrap();
    let v: serde_json::Value = serde_json::from_str(&res).unwrap();
    assert_eq!(v["total"], 3, "total reflects matches, not page size");
    assert_eq!(v["offset"], 0);
    assert_eq!(v["results"].as_array().unwrap().len(), 2);
    assert!(
        v["results"][0].get("body").is_none(),
        "metadata only — bodies must never leak through search"
    );
    assert!(v["results"][0].get("snippet").is_some());
}

#[tokio::test]
async fn search_escapes_user_text_into_script() {
    let mut f = fixture(&[r#"{"ok":true,"value":{"total":0,"results":[]}}"#]);
    let _ =
        f.ts.search("O'Brien \"quoted\" \\path", Some("work"), None, None, 0)
            .await
            .unwrap();
    let script = &f.ts.transport.calls()[0].script;
    // Quotes and backslashes escaped so the text cannot break out of the
    // JS string literal; queries are matched case-insensitively, so the
    // embedded text is the lowercased query.
    assert!(
        script.contains(r#""o'brien \"quoted\" \\path""#),
        "query not safely escaped: {script}"
    );
    assert!(script.contains(r#""work""#));
}

#[tokio::test]
async fn read_returns_full_message() {
    let mut f = fixture(&[r#"{"ok":true,"value":{"id":"m1","subject":"s","body":"full body"}}"#]);
    let res = f.ts.read("m1").await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&res).unwrap();
    assert_eq!(v["body"], "full body");
}

#[tokio::test]
async fn send_without_gate_refuses() {
    let t = MockTransport::new();
    let mut ts = MailToolset::new(t);
    ts.gate = None;
    let err = ts.send("mom@x", "Hi", "Hello!", None).await.unwrap_err();
    assert!(err.to_string().contains("gate"), "{err}");
}

#[tokio::test]
async fn send_first_call_requires_confirmation() {
    let mut f = fixture(&[]);
    let res =
        f.ts.send("mom@example.com", "Hi", "Hello!", None)
            .await
            .unwrap();
    let v: serde_json::Value = serde_json::from_str(&res).unwrap();
    assert_eq!(v["status"], "requires_confirmation");
    assert_eq!(v["payload"]["to"], "mom@example.com");
    assert_eq!(v["payload"]["subject"], "Hi");
    assert!(v["confirmation_token"].as_str().unwrap().len() > 10);
    // No transport call may happen before confirmation.
    assert_eq!(f.ts.transport.calls().len(), 0);
}

#[tokio::test]
async fn send_with_token_executes_once() {
    let mut f = fixture(&[r#"{"ok":true,"value":{"status":"sent"}}"#]);
    let first =
        f.ts.send("mom@example.com", "Hi", "Hello!", None)
            .await
            .unwrap();
    let token: String =
        serde_json::from_str::<serde_json::Value>(&first).unwrap()["confirmation_token"]
            .as_str()
            .unwrap()
            .into();

    let res =
        f.ts.send("mom@example.com", "Hi", "Hello!", Some(&token))
            .await
            .unwrap();
    let v: serde_json::Value = serde_json::from_str(&res).unwrap();
    assert_eq!(v["status"], "sent");

    // Exactly one transport call, made only after the token was accepted.
    assert_eq!(f.ts.transport.calls().len(), 1);

    // Token is single-use: replaying it re-enters confirmation, it must not
    // fire a second send.
    let replay =
        f.ts.send("mom@example.com", "Hi", "Hello!", Some(&token))
            .await
            .unwrap();
    let rv: serde_json::Value = serde_json::from_str(&replay).unwrap();
    assert_eq!(rv["status"], "requires_confirmation");
    assert_eq!(
        f.ts.transport.calls().len(),
        1,
        "replayed token must not send"
    );
}

/// Real Mail round trip — strictly read-only. Runs on macOS dev machines
/// and the macOS CI job; requires Automation permission for Mail.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn real_mail_accounts_search_read() {
    use personai_core::macos::JxaTransport;
    let mut ts = MailToolset::new(JxaTransport);

    let accounts = ts.list_accounts().await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&accounts).unwrap();
    assert!(v["accounts"].as_array().unwrap().len() >= 1);

    let res = ts.search("a", None, None, Some(5), 0).await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&res).unwrap();
    let results = v["results"].as_array().unwrap();
    assert!(v["total"].as_u64().unwrap() >= 1);
    assert!(results.iter().all(|r| r.get("body").is_none()));

    if let Some(id) = results[0]["id"].as_str() {
        let full = ts.read(id).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&full).unwrap();
        assert!(v["body"].is_string());
    }
}
