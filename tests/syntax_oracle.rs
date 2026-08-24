//! Syntax oracle (macOS only): every generated JXA script must pass
//! `osacompile -l JavaScript`, which COMPILES WITHOUT EXECUTING — no app
//! access, no TCC permission prompts. This catches syntax-dead templates on
//! a real AppleScript toolchain, complementing the OS-agnostic `balanced()`
//! characterizer in tests/common/mod.rs.

#![cfg(target_os = "macos")]

use mcp_macos::calendar::CalendarToolset;
use mcp_macos::contacts::ContactsToolset;
use mcp_macos::mail::{MailGroupBy, MailTargets, MailToolset};
use mcp_macos::messages::MessagesToolset;
use mcp_macos::reminders::RemindersToolset;
use personai_core::macos::MockTransport;

/// True when the compile oracle is present. CI macOS runners and real Macs
/// ship /usr/bin/osacompile; absence is reported, not fatal.
fn oracle_available() -> bool {
    std::path::Path::new("/usr/bin/osacompile").exists()
}

/// Compiles `script` with osacompile without executing it.
fn assert_compiles(name: &str, script: &str) {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join(format!("{name}.js"));
    let out = dir.path().join(format!("{name}.scpt"));
    std::fs::write(&src, script).unwrap();
    let status = std::process::Command::new("/usr/bin/osacompile")
        .args(["-l", "JavaScript", "-o"])
        .arg(&out)
        .arg(&src)
        .status()
        .expect("osacompile spawn");
    assert!(
        status.success(),
        "{name}: osacompile rejected script:\n{script}"
    );
}

fn mock(envelopes: &[&str]) -> MockTransport {
    let mut t = MockTransport::new();
    for e in envelopes {
        t.enqueue(e);
    }
    t
}

/// Extracts the confirmation token from a requires_confirmation payload.
fn confirm_token(res: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(res).unwrap();
    v["confirmation_token"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn mail_scripts_compile() {
    if !oracle_available() {
        eprintln!("skipping: /usr/bin/osacompile not found");
        return;
    }
    // Grouped multi-target search.
    let env = r#"{"ok":true,"value":{"total":0,"total_groups":0,"groups":[],"scanned_per_folder":{},"truncated":false}}"#;
    let mut ts = MailToolset::new(mock(&[env]));
    ts.search_multi(
        &MailTargets::Unified,
        "",
        &[],
        None,
        None,
        20,
        0,
        Some(MailGroupBy::Sender),
        5000,
        false,
    )
    .await
    .unwrap();
    let grouped = ts.transport.calls()[0].script.clone();

    // Row-mode multi-target search.
    let env2 =
        r#"{"ok":true,"value":{"total":0,"results":[],"scanned_per_folder":{},"truncated":false}}"#;
    let mut ts = MailToolset::new(mock(&[env2]));
    ts.search_multi(
        &MailTargets::Folders(vec![("W".into(), "I".into())]),
        "q",
        &[],
        None,
        None,
        10,
        0,
        None,
        5000,
        false,
    )
    .await
    .unwrap();
    let row_mode = ts.transport.calls()[0].script.clone();

    // Single-account search + accounts list.
    let mut ts = MailToolset::new(mock(&[r#"{"ok":true,"value":{"total":0,"results":[]}}"#]));
    ts.search(
        "q",
        &[],
        Some("Google"),
        None,
        None,
        None,
        Some(10),
        0,
        5000,
        true,
    )
    .await
    .unwrap();
    let single_box = ts.transport.calls()[0].script.clone();

    let mut ts = MailToolset::new(mock(&[
        r#"{"ok":true,"value":{"accounts":[{"name":"Google","email":"d@gmail.com"}]}}"#,
    ]));
    ts.list_accounts().await.unwrap();
    let accounts = ts.transport.calls()[0].script.clone();

    for (name, script) in [
        ("mail_grouped_search", grouped.as_str()),
        ("mail_row_search", row_mode.as_str()),
        ("mail_single_box_search", single_box.as_str()),
        ("mail_list_accounts", accounts.as_str()),
    ] {
        assert_compiles(name, script);
    }
}

#[tokio::test]
async fn messages_scripts_compile() {
    if !oracle_available() {
        eprintln!("skipping: /usr/bin/osacompile not found");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("tokens.json");

    let mut ts = MessagesToolset::with_gate(
        mock(&[r#"{"ok":true,"value":{"total":0,"messages":[]}}"#]),
        store.clone(),
    )
    .unwrap();
    ts.read(None, None, 0).await.unwrap();
    let read = ts.transport.calls()[0].script.clone();

    let mut ts = MessagesToolset::with_gate(
        mock(&[r#"{"ok":true,"value":{"total":0,"chats":[]}}"#]),
        store.clone(),
    )
    .unwrap();
    ts.chats(None, 0).await.unwrap();
    let chats = ts.transport.calls()[0].script.clone();

    // send_expr runs in the gate's execute phase.
    let mut ts =
        MessagesToolset::with_gate(mock(&[r#"{"ok":true,"value":{"status":"sent"}}"#]), store)
            .unwrap();
    let token = confirm_token(&ts.send("+15550001111", "Hi!", None).await.unwrap());
    ts.send("+15550001111", "Hi!", Some(&token)).await.unwrap();
    let send = ts.transport.calls()[0].script.clone();

    for (name, script) in [
        ("messages_read", read.as_str()),
        ("messages_chats", chats.as_str()),
        ("messages_send", send.as_str()),
    ] {
        assert_compiles(name, script);
    }
}

#[tokio::test]
async fn contacts_scripts_compile() {
    if !oracle_available() {
        eprintln!("skipping: /usr/bin/osacompile not found");
        return;
    }
    let mut ts = ContactsToolset::new(mock(&[r#"{"ok":true,"value":{"total":0,"contacts":[]}}"#]));
    ts.search("O'Brien", Some(5), 2).await.unwrap();
    assert_compiles("contacts_search", &ts.transport.calls()[0].script);
}

#[tokio::test]
async fn reminders_scripts_compile() {
    if !oracle_available() {
        eprintln!("skipping: /usr/bin/osacompile not found");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("tokens.json");

    let mut ts = RemindersToolset::with_gate(
        mock(&[r#"{"ok":true,"value":{"lists":[]}}"#]),
        store.clone(),
    )
    .unwrap();
    ts.list_lists().await.unwrap();
    let lists = ts.transport.calls()[0].script.clone();

    let mut ts = RemindersToolset::with_gate(
        mock(&[
            r#"{"ok":true,"value":{"total":0,"offset":0,"limit":20,"reminders":[]}}"#,
            r#"{"ok":true,"value":{"id":"r9"}}"#,
            r#"{"ok":true,"value":{"id":"r1"}}"#,
        ]),
        store,
    )
    .unwrap();
    ts.read(Some("Work".into()), false, None, 0).await.unwrap();
    let read = ts.transport.calls()[0].script.clone();
    let token = confirm_token(
        &ts.create(
            "Call bank",
            Some("Work"),
            Some("2026-08-25T09:00:00Z"),
            Some("notes"),
            None,
        )
        .await
        .unwrap(),
    );
    ts.create(
        "Call bank",
        Some("Work"),
        Some("2026-08-25T09:00:00Z"),
        Some("notes"),
        Some(&token),
    )
    .await
    .unwrap();
    let create = ts.transport.calls()[1].script.clone();
    let token = confirm_token(&ts.complete("r1", None).await.unwrap());
    ts.complete("r1", Some(&token)).await.unwrap();
    let complete = ts.transport.calls()[2].script.clone();

    for (name, script) in [
        ("reminders_lists", lists.as_str()),
        ("reminders_read", read.as_str()),
        ("reminders_create", create.as_str()),
        ("reminders_complete", complete.as_str()),
    ] {
        assert_compiles(name, script);
    }
}

#[tokio::test]
async fn calendar_scripts_compile() {
    if !oracle_available() {
        eprintln!("skipping: /usr/bin/osacompile not found");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut ts = CalendarToolset::with_gate(
        mock(&[
            r#"{"ok":true,"value":{"total":0,"events":[]}}"#,
            r#"{"ok":true,"value":{"id":"e2","calendar":"Work"}}"#,
            r#"{"ok":true,"value":{"id":"e1"}}"#,
            r#"{"ok":true,"value":{"id":"e1"}}"#,
        ]),
        dir.path().join("tokens.json"),
    )
    .unwrap();
    ts.read(None, None, None, 0).await.unwrap();
    let read = ts.transport.calls()[0].script.clone();
    let token = confirm_token(
        &ts.create(
            "Interview",
            "2026-08-25T09:00:00Z",
            "2026-08-25T10:00:00Z",
            Some("Work"),
            Some("Room 4"),
            None,
            None,
        )
        .await
        .unwrap(),
    );
    ts.create(
        "Interview",
        "2026-08-25T09:00:00Z",
        "2026-08-25T10:00:00Z",
        Some("Work"),
        Some("Room 4"),
        None,
        Some(&token),
    )
    .await
    .unwrap();
    let create = ts.transport.calls()[1].script.clone();
    let token = confirm_token(
        &ts.update("e1", Some("New title"), None, None, None, None, None)
            .await
            .unwrap(),
    );
    ts.update(
        "e1",
        Some("New title"),
        None,
        None,
        None,
        None,
        Some(&token),
    )
    .await
    .unwrap();
    let update = ts.transport.calls()[2].script.clone();
    let token = confirm_token(&ts.delete("e1", None).await.unwrap());
    ts.delete("e1", Some(&token)).await.unwrap();
    let delete = ts.transport.calls()[3].script.clone();

    for (name, script) in [
        ("calendar_read", read.as_str()),
        ("calendar_create", create.as_str()),
        ("calendar_update", update.as_str()),
        ("calendar_delete", delete.as_str()),
    ] {
        assert_compiles(name, script);
    }
}
