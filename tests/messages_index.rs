//! Contract tests for the Messages corpus index: composed migrations,
//! sync driver (rowid watermark), and FTS search.

use personai_core::index::IndexHandle;
use serde_json::{json, Value};

fn handle(name: &str) -> IndexHandle {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    std::mem::forget(dir);
    IndexHandle::open(path, mcp_macos::index_schema::INDEX_MIGRATIONS).unwrap()
}

// --- Cycle A: one database file serves BOTH surfaces -----------------------

#[test]
fn composed_migrations_create_mail_and_messages_tables() {
    let h = handle("shared.db");
    assert_eq!(h.schema_version().unwrap(), 2, "mail v1 + messages v1");
    let names: Vec<Value> = h
        .query(
            "SELECT name FROM sqlite_master WHERE type='table' \
             AND name IN ('mail_messages','msg_messages') ORDER BY name",
            &[],
        )
        .unwrap();
    let got: Vec<String> =
        names.iter().map(|r| r["name"].as_str().unwrap().to_string()).collect();
    assert_eq!(got, ["mail_messages", "msg_messages"]);
}

#[test]
fn stale_opener_with_mail_only_list_is_rejected_on_v2_file() {
    // A build that only knows mail must NOT silently open a v2 file whose
    // schema it cannot maintain — FutureSchema protects both directions.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v2.db");
    std::mem::forget(dir);
    IndexHandle::open(&path, mcp_macos::index_schema::INDEX_MIGRATIONS).unwrap();
    let err = IndexHandle::open(&path, mcp_macos::mail_index::MAIL_MIGRATIONS).unwrap_err();
    assert!(matches!(
        err,
        personai_core::index::IndexError::FutureSchema { .. }
    ));
}

// --- Cycle B: incremental sync by chat.db ROWID watermark ------------------

use mcp_macos::messages_index::{sync_expr, sync_messages};
use personai_core::macos::MockTransport;

fn envelope(rows: Value) -> String {
    // Mirror production: the JXA mapper returns a preserialized STRING.
    json!({"ok": true, "value": json!({"rows": rows}).to_string()}).to_string()
}

#[test]
fn sync_builder_filters_tapbacks_and_pages_by_rowid() {
    let s = sync_expr(41, 2000);
    assert!(s.contains("m.ROWID > 41"), "{s}");
    assert!(s.contains("associated_message_type,0)=0"), "{s}");
    assert!(s.contains("LIMIT 2000"), "{s}");
    // doShellScript emits CR endings — the mapper must be robust.
    assert!(s.contains("split(/\\r\\n|\\r|\\n/)"), "{s}");
}

#[tokio::test]
async fn sync_batches_until_empty_and_records_watermark() {
    let h = handle("sync.db");
    let mut t = MockTransport::new();
    t.enqueue(&envelope(json!([
        {"i": 40, "c": "+17038140603", "f": 0, "s": "+17038140603", "t": "hey",      "d": "2026-07-01T00:00:00Z"},
        {"i": 41, "c": "chat123",         "f": 1, "s": "",             "t": "sent!", "d": "2026-07-02T00:00:00Z"}
    ])));
    t.enqueue(&envelope(json!([]))); // drained
    let out = sync_messages(&mut t, &h, false, 2000).await.unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["synced"], 2);
    let (wm, _, counts_raw) = h.watermark("messages", "chatdb").unwrap().unwrap();
    assert_eq!(wm, "41", "watermark = newest source ROWID");
    let counts: Value = serde_json::from_str(&counts_raw).unwrap();
    assert_eq!(counts["count"], 2);
}

#[tokio::test]
async fn full_sync_discards_stale_rows() {
    let h = handle("full.db");
    h.upsert(
        "msg_messages",
        &["rowid_src","chat_identifier","is_from_me","sender","text","date_iso","fetched_at"],
        &["rowid_src"],
        &[json!({"rowid_src": 999, "chat_identifier":"ghost","is_from_me":0,
                 "sender":"","text":"deleted long ago","date_iso":"2020-01-01T00:00:00Z",
                 "fetched_at":1})],
    )
    .unwrap();
    let mut t = MockTransport::new();
    t.enqueue(&envelope(json!([
        {"i": 5, "c": "a@x", "f": 0, "s": "a@x", "t": "fresh", "d": "2026-08-01T00:00:00Z"}
    ])));
    t.enqueue(&envelope(json!([])));
    sync_messages(&mut t, &h, true, 2000).await.unwrap();
    let ids: Vec<Value> = h.query("SELECT rowid_src FROM msg_messages", &[]).unwrap();
    assert_eq!(ids.len(), 1);
    assert_eq!(ids[0]["rowid_src"], 5);
}

#[tokio::test]
async fn delta_sync_starts_after_stored_watermark() {
    let h = handle("delta.db");
    h.set_watermark("messages", "chatdb", "10", &json!({})).unwrap();
    let mut t = MockTransport::new();
    t.enqueue(&envelope(json!([])));
    sync_messages(&mut t, &h, false, 2000).await.unwrap();
    let script = &t.calls()[0].script;
    assert!(script.contains("m.ROWID > 10"), "{script}");
}
