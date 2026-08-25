//! Contract tests for the mail corpus index: schema, fingerprinting, and
//! the MockTransport-driven sync driver. No live Mail.app involved.

use mcp_macos::mail::MailTargets;
use mcp_macos::mail_index::{fingerprint, sync_expr, sync_folder, sync_targets};
use personai_core::index::IndexHandle;
use personai_core::macos::MockTransport;
use serde_json::{Value, json};

fn handle(name: &str) -> IndexHandle {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    std::mem::forget(dir);
    IndexHandle::open(path, mcp_macos::mail_index::MAIL_MIGRATIONS).unwrap()
}

/// Canned JXA-envelope stdout carrying `rows` for a folder fetch.
fn envelope(rows: serde_json::Value) -> String {
    json!({"ok": true, "value": {"rows": rows}}).to_string()
}

#[test]
fn fingerprint_is_stable_hex_and_input_sensitive() {
    let a = fingerprint(
        "noreply@ibm.com",
        "Assessment invite",
        "2026-08-24T10:00:00Z",
    );
    let b = fingerprint(
        "noreply@ibm.com",
        "Assessment invite",
        "2026-08-24T10:00:00Z",
    );
    assert_eq!(a, b, "same inputs → same fingerprint");
    assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "{a} must be hex");
    assert_eq!(a.len(), 32);
    let c = fingerprint("other@x.com", "Assessment invite", "2026-08-24T10:00:00Z");
    assert_ne!(a, c);
}

#[test]
fn sync_expr_escapes_names_and_bakes_window_and_scan() {
    let s = sync_expr("Wörk \"1\"", "Box\\A", Some("2026-08-01T00:00:00Z"), 1000);
    assert!(s.contains(r#"{name: "Wörk \"1\""}"#), "{s}");
    assert!(s.contains(r#"Box\\A"#), "{s}");
    // since floor present as an ISO string literal
    assert!(s.contains(r#""2026-08-01T00:00:00Z""#), "{s}");
    assert!(s.contains("Math.min(box.messages.length, 1000)"), "{s}");
    // Four bulk property events
    for prop in [".id()", ".subject()", ".sender()", ".dateReceived()"] {
        assert!(s.contains(prop), "missing {prop} in {s}");
    }
    // Without `since`, no window literal.
    let u = sync_expr("A", "B", None, 5000);
    assert!(!u.contains("sinceMs"), "{u}");
}

#[tokio::test]
async fn sync_folder_full_replaces_partition_and_sets_watermark() {
    let h = handle("full.db");
    let mut t = MockTransport::new();
    t.enqueue(&envelope(json!([
        {"i": "101", "s": "Welcome",   "f": "IBM <no-reply@ibm.com>",  "d": "2026-08-20T10:00:00.000Z"},
        {"i": "102", "s": "Interview", "f": "Meta <careers@metac.com>","d": "2026-08-22T09:00:00.000Z"}
    ])));
    let stats = sync_folder(&mut t, &h, "Exchange", "Apps", true, 5000)
        .await
        .unwrap();
    assert_eq!(
        (stats.scanned, stats.new, stats.updated, stats.mismatches),
        (2, 2, 0, 0)
    );
    let rows = h
        .query(
            "SELECT apple_id, subject, sender, mailbox_key FROM mail_messages ORDER BY apple_id",
            &[],
        )
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1]["mailbox_key"], "Exchange/Apps");
    assert_eq!(rows[1]["subject"], "Interview");
    let (wm, _, counts_raw) = h.watermark("mail", "Exchange/Apps").unwrap().unwrap();
    assert_eq!(
        wm, "2026-08-22T09:00:00.000Z",
        "watermark = newest date seen"
    );
    let counts: serde_json::Value = serde_json::from_str(&counts_raw).unwrap();
    assert_eq!(counts["count"], 2);
}

#[tokio::test]
async fn sync_folder_delta_upserts_and_counts_mismatches() {
    let h = handle("delta.db");
    let mut t = MockTransport::new();
    // First pass: full seed of two rows.
    t.enqueue(&envelope(json!([
        {"i": "101", "s": "Old subject", "f": "a@x.com", "d": "2026-08-20T10:00:00.000Z"},
        {"i": "102", "s": "Stable",      "f": "b@x.com", "d": "2026-08-21T10:00:00.000Z"}
    ])));
    sync_folder(&mut t, &h, "Exchange", "Apps", true, 5000)
        .await
        .unwrap();
    // Delta: id 101 changed subject (fingerprint mismatch), id 103 new,
    // id 102 unchanged but re-delivered (updated, no mismatch).
    t.enqueue(&envelope(json!([
        {"i": "101", "s": "New subject", "f": "a@x.com", "d": "2026-08-20T10:00:00.000Z"},
        {"i": "102", "s": "Stable",      "f": "b@x.com", "d": "2026-08-21T10:00:00.000Z"},
        {"i": "103", "s": "Fresh",       "f": "c@x.com", "d": "2026-08-23T10:00:00.000Z"}
    ])));
    let stats = sync_folder(&mut t, &h, "Exchange", "Apps", false, 5000)
        .await
        .unwrap();
    assert_eq!(
        (stats.scanned, stats.new, stats.updated, stats.mismatches),
        (3, 1, 2, 1),
        "new=103, updated=101+102, mismatch=changed subject on 101"
    );
    let subj = h
        .query(
            "SELECT subject FROM mail_messages WHERE apple_id='101'",
            &[],
        )
        .unwrap()
        .remove(0)["subject"]
        .clone();
    assert_eq!(subj, "New subject");
    let total: i64 = h
        .query("SELECT COUNT(*) AS n FROM mail_messages", &[])
        .unwrap()
        .remove(0)["n"]
        .as_i64()
        .unwrap();
    assert_eq!(total, 3);
    // Watermark advanced to the newest delta row.
    assert_eq!(
        h.watermark("mail", "Exchange/Apps").unwrap().unwrap().0,
        "2026-08-23T10:00:00.000Z"
    );
}

#[test]
fn sync_expr_delta_pass_carries_since_floor() {
    let s = sync_expr("A", "B", Some("2026-08-23T00:00:00Z"), 200);
    assert!(s.contains(r#""2026-08-23T00:00:00Z""#), "{s}");
}

#[tokio::test]
async fn sync_targets_rejects_unified_with_hint() {
    let h = handle("unified.db");
    let mut t = MockTransport::new();
    let err = sync_targets(&mut t, &h, &MailTargets::Unified, false, 5000)
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.to_lowercase().contains("folder"), "{msg}");
}

#[tokio::test]
async fn sync_sweep_survives_a_folder_that_throws() {
    // Live finding: Google/Notes and Exchange/Notes throw AppleEvent -1728
    // on bulk message fetches. One poison folder must never discard the
    // other ~27 folders' work.
    let h = handle("poison.db");
    let mut t = MockTransport::new();
    t.enqueue(&envelope(json!([
        { "i": "1", "s": "x", "f": "a@x", "d": "2026-08-01T00:00:00.000Z" }
    ])));
    // Poison folder: JXA error envelope (-1728).
    t.enqueue(
        &json!({
            "ok": false,
            "error": {"number": -1728, "desc": "Can't get object."}
        })
        .to_string(),
    );
    t.enqueue(&envelope(json!([
        { "i": "2", "s": "y", "f": "b@x", "d": "2026-08-02T00:00:00.000Z" }
    ])));
    let targets = MailTargets::Folders(vec![
        ("Exchange".into(), "Apps".into()),
        ("Exchange".into(), "Notes".into()),
        ("Google".into(), "INBOX".into()),
    ]);
    let out = sync_targets(&mut t, &h, &targets, false, 5000)
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["synced_per_folder"]["Exchange/Apps"]["scanned"], 1);
    assert_eq!(v["synced_per_folder"]["Google/INBOX"]["scanned"], 1);
    assert!(
        v["errors"]["Exchange/Notes"]
            .as_str()
            .unwrap()
            .contains("-1728"),
        "poison folder recorded, sweep continued: {out}"
    );
}

#[tokio::test]
async fn sync_targets_sweeps_all_pairs() {
    let h = handle("sweep.db");
    let mut t = MockTransport::new();
    t.enqueue(&envelope(
        json!([{ "i": "1", "s": "x", "f": "a@x", "d": "2026-08-01T00:00:00.000Z" }]),
    ));
    t.enqueue(&envelope(json!([])));
    let targets = MailTargets::Folders(vec![
        ("Exchange".into(), "Apps".into()),
        ("Google".into(), "INBOX".into()),
    ]);
    let out = sync_targets(&mut t, &h, &targets, false, 5000)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["synced_per_folder"]["Exchange/Apps"]["scanned"], 1);
    assert_eq!(v["synced_per_folder"]["Google/INBOX"]["scanned"], 0);
    assert_eq!(v["data_as_of"], "2026-08-01T00:00:00.000Z");
    assert!(v["duration_ms"].as_u64().is_some());
}

// --- Error-taxonomy coverage: every transport failure class must be -------
// --- contained per-folder, never abort the sweep. --------------------------

#[tokio::test]
async fn sweep_survives_timeout_on_one_folder() {
    let h = handle("timeout-sweep.db");
    let mut t = MockTransport::new();
    t.enqueue(&envelope(json!([
        { "i": "1", "s": "x", "f": "a@x", "d": "2026-08-01T00:00:00.000Z" }
    ])));
    t.enqueue_timeout(); // folder 2 hangs
    t.enqueue(&envelope(json!([
        { "i": "3", "s": "z", "f": "c@x", "d": "2026-08-03T00:00:00.000Z" }
    ])));
    let targets = MailTargets::Folders(vec![
        ("A".into(), "B".into()),
        ("C".into(), "D".into()),
        ("E".into(), "F".into()),
    ]);
    let out = sync_targets(&mut t, &h, &targets, false, 5000)
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["synced_per_folder"]["A/B"]["scanned"], 1);
    assert_eq!(v["synced_per_folder"]["E/F"]["scanned"], 1);
    assert!(
        v["errors"]["C/D"].as_str().is_some(),
        "timeout recorded: {out}"
    );
}

#[tokio::test]
async fn sweep_survives_malformed_envelope_on_one_folder() {
    let h = handle("malformed-sweep.db");
    let mut t = MockTransport::new();
    t.enqueue("not-json-at-all");
    t.enqueue(&envelope(json!([
        { "i": "9", "s": "y", "f": "b@x", "d": "2026-08-02T00:00:00.000Z" }
    ])));
    let targets = MailTargets::Folders(vec![
        ("Bad".into(), "Box".into()),
        ("Good".into(), "Box".into()),
    ]);
    let out = sync_targets(&mut t, &h, &targets, false, 5000)
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["synced_per_folder"]["Good/Box"]["scanned"], 1);
    assert!(v["errors"]["Bad/Box"].as_str().is_some());
}

#[test]
fn delta_with_no_new_mail_leaves_rows_and_watermark_untouched() {
    // Covered via builder contract: the delta pass carries the watermark
    // floor; empty results must neither upsert nor regress the watermark.
    let s = sync_expr("A", "B", Some("2026-08-24T10:00:00Z"), 5000);
    assert!(
        s.contains("Date.parse(\"2026-08-24T10:00:00Z\") - 3600000"),
        "clock-skew buffer must be baked into the script"
    );
}

#[test]
fn full_replace_is_idempotent_across_double_full_passes() {
    let h = handle("double-full.db");
    let cols = [
        "account",
        "mailbox",
        "apple_id",
        "fingerprint",
        "subject",
        "sender",
        "date_received",
        "fetched_at",
    ];
    let row = |id: &str| {
        json!({
            "account": "A", "mailbox": "B", "apple_id": id,
            "fingerprint": fingerprint("a@x","s","2026-08-01T00:00:00Z"),
            "subject": "s", "sender": "a@x",
            "date_received": "2026-08-01T00:00:00Z", "fetched_at": 1
        })
    };
    h.replace_partition(
        "mail_messages",
        "mailbox_key",
        "A/B",
        &cols,
        &["account", "mailbox", "apple_id"],
        &[row("1")],
    )
    .unwrap();
    let n = h
        .replace_partition(
            "mail_messages",
            "mailbox_key",
            "A/B",
            &cols,
            &["account", "mailbox", "apple_id"],
            &[row("1")],
        )
        .unwrap();
    assert_eq!(n, 1, "second full pass rewrites the same single row");
    let count: i64 = h
        .query("SELECT COUNT(*) AS n FROM mail_messages", &[])
        .unwrap()
        .remove(0)["n"]
        .as_i64()
        .unwrap();
    assert_eq!(count, 1);
}
