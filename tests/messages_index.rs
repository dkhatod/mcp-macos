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

// --- Cycle B/C placeholders get their own cycles below ---------------------
