//! Mail toolset contract tests. All run on every OS via `MockTransport`.

use mcp_macos::mail::{MailTargets, MailToolset};
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
        f.ts.search("application", None, None, None, Some(2), 0)
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
        f.ts.search(
            "O'Brien \"quoted\" \\path",
            Some("work"),
            None,
            None,
            None,
            0,
        )
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
    let err = ts
        .send("mom@x", "Hi", "Hello!", None, None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("gate"), "{err}");
}

#[tokio::test]
async fn send_first_call_requires_confirmation() {
    let mut f = fixture(&[]);
    let res =
        f.ts.send("mom@example.com", "Hi", "Hello!", None, None)
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
        f.ts.send("mom@example.com", "Hi", "Hello!", None, None)
            .await
            .unwrap();
    let token: String =
        serde_json::from_str::<serde_json::Value>(&first).unwrap()["confirmation_token"]
            .as_str()
            .unwrap()
            .into();

    let res =
        f.ts.send("mom@example.com", "Hi", "Hello!", None, Some(&token))
            .await
            .unwrap();
    let v: serde_json::Value = serde_json::from_str(&res).unwrap();
    assert_eq!(v["status"], "sent");

    // Exactly one transport call, made only after the token was accepted.
    assert_eq!(f.ts.transport.calls().len(), 1);

    // Token is single-use: replaying it re-enters confirmation, it must not
    // fire a second send.
    let replay =
        f.ts.send("mom@example.com", "Hi", "Hello!", None, Some(&token))
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

/// Real Mail round trip — strictly read-only. Skips on hosts without a
/// Mail automation grant (CI runners cannot approve the TCC prompt).
#[cfg(target_os = "macos")]
#[tokio::test]
async fn real_mail_accounts_search_read() {
    use personai_core::macos::JxaTransport;
    let mut ts = MailToolset::new(JxaTransport);

    let accounts = match ts.list_accounts().await {
        Ok(a) => a,
        Err(e)
            if e.to_string().contains("permission denied")
                || e.to_string().contains("timed out") =>
        {
            eprintln!("skipping: no Mail automation grant on this host ({e})");
            return;
        }
        Err(e) => panic!("list_accounts: {e}"),
    };
    let v: serde_json::Value = serde_json::from_str(&accounts).unwrap();
    assert!(v["accounts"].as_array().unwrap().len() >= 1);

    let res = ts.search("a", None, None, None, Some(5), 0).await.unwrap();
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

#[tokio::test]
async fn list_accounts_returns_identity_fields() {
    let mut f = fixture(&[
        r#"{"ok":true,"value":[{"name":"Google","email":"d@gmail.com","accountType":"imap","enabled":true}]}"#,
    ]);
    let res = f.ts.list_accounts().await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&res).unwrap();
    assert_eq!(v["accounts"][0]["name"], "Google");
    assert_eq!(v["accounts"][0]["email"], "d@gmail.com");
    assert_eq!(v["accounts"][0]["accountType"], "imap");
}

#[tokio::test]
async fn list_mailboxes_returns_counts_and_respects_filter() {
    let mut f = fixture(&[
        r#"{"ok":true,"value":{"mailboxes":[{"account":"Google","name":"INBOX","count":120},{"account":"Google","name":"Work","count":7}]}}"#,
    ]);
    let res = f.ts.list_mailboxes(Some("Google".into())).await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&res).unwrap();
    assert_eq!(v["mailboxes"].as_array().unwrap().len(), 2);
    assert_eq!(v["mailboxes"][0]["count"], 120);
    let script = &f.ts.transport.calls()[0].script;
    assert!(script.contains(r#""Google""#), "{script}");
}

#[tokio::test]
async fn search_targets_named_mailbox_when_given() {
    let mut f = fixture(&[r#"{"ok":true,"value":{"total":0,"results":[]}}"#]);
    let _ =
        f.ts.search("x", Some("Google"), Some("Work".into()), None, None, 0)
            .await
            .unwrap();
    let script = &f.ts.transport.calls()[0].script;
    assert!(
        script.contains(r#""Work""#),
        "mailbox not targeted: {script}"
    );
}

#[tokio::test]
async fn list_mailboxes_detailed_returns_counts_and_last_activity() {
    let mut f = fixture(&[r#"{"ok":true,"value":{"mailboxes":[
        {"account":"Google","name":"INBOX","count":5,"last_activity":"2026-08-20T10:00:00.000Z"},
        {"account":"Google","name":"Trash","count":0,"last_activity":null}
    ]}}"#]);
    let res =
        f.ts.list_mailboxes_detailed(Some("Google".into()))
            .await
            .unwrap();
    let v: serde_json::Value = serde_json::from_str(&res).unwrap();
    assert_eq!(v["mailboxes"][0]["count"], 5);
    assert_eq!(
        v["mailboxes"][0]["last_activity"],
        "2026-08-20T10:00:00.000Z"
    );
    assert!(
        v["mailboxes"][1]["last_activity"].is_null(),
        "empty boxes report null last_activity"
    );
    let script = &f.ts.transport.calls()[0].script;
    assert!(script.contains(r#""Google""#), "{script}");
}

#[tokio::test]
async fn search_multi_merges_folders_paginates_and_strips_bodies() {
    // One script scans both folders; the mock returns what the real JXA
    // merge would produce: interleaved folders, newest first, bodies in.
    let value = json!({
        "total": 3,
        "results": [
            {"id":"m2","subject":"s2","from":"b@x","date":"2026-08-19T09:00:00Z","snippet":"","folder":"Google/Inbox"},
            {"id":"m1","subject":"s1","from":"a@x","date":"2026-08-18T09:00:00Z","snippet":"sn1","folder":"Work/Archive","body":"BODY1"}
        ],
        "scanned_per_folder": {"Work/Inbox": 12, "Google/Inbox": 9},
        "truncated": false
    });
    let env = format!(r#"{{"ok":true,"value":{value}}}"#);
    let mut f = fixture(&[&env]);
    let res =
        f.ts.search_multi(
            &MailTargets::Folders(vec![
                ("Work".into(), "Inbox".into()),
                ("Google".into(), "Inbox".into()),
            ]),
            "invoice",
            None,
            2,
            1,
        )
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&res).unwrap();
    assert_eq!(v["total"], 3, "total is pre-pagination across all folders");
    assert_eq!(v["offset"], 1);
    assert_eq!(v["limit"], 2);
    let page = v["results"].as_array().unwrap();
    assert_eq!(page.len(), 2);
    assert_eq!(page[0]["id"], "m2", "global date-desc merge order");
    assert_eq!(page[1]["id"], "m1");
    assert_eq!(page[1]["folder"], "Work/Archive", "folder tag survives");
    assert!(page.iter().all(|r| r.get("body").is_none()));
    assert_eq!(v["scanned_per_folder"]["Work/Inbox"], 12);
    assert_eq!(v["truncated"], false);
    // Exactly ONE script for both folders, carrying budget + scan caps.
    assert_eq!(f.ts.transport.calls().len(), 1);
    let script = &f.ts.transport.calls()[0].script;
    assert!(script.contains("const BUDGET_MS = 25000;"), "{script}");
    assert!(script.contains(r#"{a: "Work", b: "Inbox"}"#), "{script}");
}

#[tokio::test]
async fn search_multi_reports_truncation_passthrough() {
    let env = r#"{"ok":true,"value":{"total":7,"results":[{"id":"m1","subject":"s","from":"a@x","date":"2026-08-19T09:00:00Z","snippet":"","folder":"W/I"}],"scanned_per_folder":{"W/I":10},"truncated":true}}"#;
    let mut f = fixture(&[env]);
    let res =
        f.ts.search_multi(
            &MailTargets::Folders(vec![("W".into(), "I".into())]),
            "q",
            None,
            50,
            0,
        )
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&res).unwrap();
    assert_eq!(v["truncated"], true, "budget truncation is passed through");
    assert_eq!(v["total"], 7);
}

#[tokio::test]
async fn forward_without_gate_refuses() {
    let t = MockTransport::new();
    let mut ts = MailToolset::new(t);
    ts.gate = None;
    let err = ts.forward("1", "mom@x", None, None).await.unwrap_err();
    assert!(err.to_string().contains("gate"), "{err}");
}

#[tokio::test]
async fn reply_without_gate_refuses() {
    let t = MockTransport::new();
    let mut ts = MailToolset::new(t);
    ts.gate = None;
    let err = ts.reply("1", "text", None).await.unwrap_err();
    assert!(err.to_string().contains("gate"), "{err}");
}

#[tokio::test]
async fn forward_confirm_then_execute_native_verb() {
    let sent = r#"{"ok":true,"value":{"status":"forwarded"}}"#;
    let mut f = fixture(&[sent]);
    let first =
        f.ts.forward("42", "boss@x", Some("See attached \"doc\""), None)
            .await
            .unwrap();
    let fv: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert_eq!(fv["status"], "requires_confirmation");
    assert_eq!(fv["payload"]["id"], "42");
    assert_eq!(fv["payload"]["to"], "boss@x");
    assert_eq!(fv["payload"]["comment"], "See attached \"doc\"");
    assert!(fv["confirmation_token"].as_str().unwrap().len() > 10);
    assert_eq!(f.ts.transport.calls().len(), 0, "no script before token");

    let token = fv["confirmation_token"].as_str().unwrap();
    let second =
        f.ts.forward("42", "boss@x", Some("See attached \"doc\""), Some(token))
            .await
            .unwrap();
    let sv: serde_json::Value = serde_json::from_str(&second).unwrap();
    assert_eq!(sv["status"], "forwarded");
    assert_eq!(f.ts.transport.calls().len(), 1);
    let script = &f.ts.transport.calls()[0].script;
    assert!(script.contains(".forward({to:"), "{script}");
    assert!(script.contains(r#""boss@x""#), "{script}");
    // Interpolated comment is js_str-escaped into the script.
    assert!(
        script.contains(r#"fw.comment = "See attached \"doc\"";"#),
        "{script}"
    );
}

#[tokio::test]
async fn reply_confirm_then_execute_native_verb() {
    let sent = r#"{"ok":true,"value":{"status":"replied"}}"#;
    let mut f = fixture(&[sent]);
    let first = f.ts.reply("7", "Here you \"go\"", None).await.unwrap();
    let fv: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert_eq!(fv["status"], "requires_confirmation");
    assert_eq!(fv["payload"]["id"], "7");
    assert_eq!(fv["payload"]["body"], "Here you \"go\"");
    assert_eq!(f.ts.transport.calls().len(), 0);

    let token = fv["confirmation_token"].as_str().unwrap();
    let second =
        f.ts.reply("7", "Here you \"go\"", Some(token))
            .await
            .unwrap();
    let sv: serde_json::Value = serde_json::from_str(&second).unwrap();
    assert_eq!(sv["status"], "replied");
    assert_eq!(f.ts.transport.calls().len(), 1);
    let script = &f.ts.transport.calls()[0].script;
    assert!(script.contains(".reply()"), "{script}");
    assert!(
        script.contains(r#"rp.content = "Here you \"go\"";"#),
        "{script}"
    );
}

#[tokio::test]
async fn send_passes_from_through_only_when_given() {
    let sent = r#"{"ok":true,"value":{"status":"sent"}}"#;
    let mut f = fixture(&[sent, sent]);

    // Default identity: confirm + execute; no account-matching clause.
    let c1 =
        f.ts.send("mom@x", "Hi", "Hello!", None, None)
            .await
            .unwrap();
    let t1 = token_of(&c1);
    f.ts.send("mom@x", "Hi", "Hello!", None, Some(&t1))
        .await
        .unwrap();

    // Explicit identity: payload carries from; the script matches it
    // case-insensitively by name or primary email.
    let c2 =
        f.ts.send("mom@x", "Hi", "Hello!", Some("Work"), None)
            .await
            .unwrap();
    let cv: serde_json::Value = serde_json::from_str(&c2).unwrap();
    assert_eq!(cv["payload"]["from"], "Work");
    assert!(cv["note"].as_str().unwrap().contains("from"));
    let t2 = token_of(&c2);
    f.ts.send("mom@x", "Hi", "Hello!", Some("Work"), Some(&t2))
        .await
        .unwrap();

    assert_eq!(f.ts.transport.calls().len(), 2);
    let plain = &f.ts.transport.calls()[0].script;
    assert!(
        !plain.contains("emailAddresses"),
        "no identity clause: {plain}"
    );
    let identified = &f.ts.transport.calls()[1].script;
    assert!(
        identified.contains(r#"want = "Work".toLowerCase()"#),
        "{identified}"
    );
    assert!(identified.contains("emailAddresses"), "{identified}");
    assert!(identified.contains("msg.account = acct"), "{identified}");
}

/// Extracts the confirmation token from a requires_confirmation payload.
fn token_of(res: &str) -> String {
    serde_json::from_str::<serde_json::Value>(res).unwrap()["confirmation_token"]
        .as_str()
        .unwrap()
        .to_string()
}
