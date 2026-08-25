//! Contract tests for the `mail_read` body cache: hit skips transport,
//! miss writes through, prune evicts oldest beyond the cap.

use mcp_macos::mail::MailToolset;
use mcp_macos::mail_index::{cache_get, cache_put, prune_bodies};
use personai_core::index::IndexHandle;
use personai_core::macos::MockTransport;
use serde_json::{Value, json};

fn handle(name: &str) -> IndexHandle {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    std::mem::forget(dir);
    IndexHandle::open(path, mcp_macos::mail_index::MAIL_MIGRATIONS).unwrap()
}

fn toolset() -> MailToolset<MockTransport> {
    MailToolset::new(MockTransport::new())
}

#[tokio::test]
async fn cache_hit_skips_transport_and_flags_cached() {
    let h = handle("hit.db");
    cache_put(
        &h,
        "Exchange/Apps|42",
        "Assessment invite",
        "IBM <talent@ibm.com>",
        "2026-08-20T10:00:00.000Z",
        "Please complete your assessment.",
        false,
    )
    .unwrap();
    let mut ts = toolset();
    let out = ts
        .read_cached("42", Some("Exchange/Apps"), &h)
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["cached"], true);
    assert_eq!(v["body"], "Please complete your assessment.");
    assert_eq!(v["subject"], "Assessment invite");
    assert_eq!(v["id"], "42");
    // Zero transport activity: every recorded call would be a live fetch.
    assert_eq!(ts.transport.calls().len(), 0);
}

#[tokio::test]
async fn cache_miss_reads_live_then_writes_through() {
    let h = handle("miss.db");
    let envelope = json!({
        "ok": true,
        "value": {
            "id": "77", "subject": "Offer call", "from": "Meta <careers@meta.com>",
            "date": "2026-08-23T12:00:00.000Z", "body": "Congratulations!",
            "body_truncated": false
        }
    })
    .to_string();
    let mut ts = toolset();
    ts.transport.enqueue(&envelope);
    let out = ts.read_cached("77", None, &h).await.unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["body"], "Congratulations!");
    assert_eq!(v["cached"], true, "write-through then serve from cache");
    // Second read consumes no new transport call: served from mail_bodies.
    assert_eq!(ts.transport.calls().len(), 1);
}

#[test]
fn prune_evicts_oldest_beyond_cap() {
    let h = handle("prune.db");
    for i in 0..10 {
        cache_put(
            &h,
            &format!("k|{i}"),
            "s",
            "f",
            "d",
            &"x".repeat(1000),
            false,
        )
        .unwrap();
        // Ensure distinct fetched_at ordering even at coarse clock precision.
        h.query(
            "UPDATE mail_bodies SET fetched_at = ?1 WHERE cache_key = ?2",
            &[json!(1_700_000_000i64 + i), json!(format!("k|{i}"))],
        )
        .unwrap();
    }
    // Cap below total (10_000 bytes): keeps newest 6k worth → evicts 4 oldest.
    prune_bodies(&h, 6_500).unwrap();
    let n: i64 = h
        .query("SELECT COUNT(*) AS n FROM mail_bodies", &[])
        .unwrap()
        .remove(0)["n"]
        .as_i64()
        .unwrap();
    assert_eq!(n, 6, "oldest rows evicted to fit cap");
    let remaining: Vec<String> = h
        .query("SELECT cache_key FROM mail_bodies ORDER BY cache_key", &[])
        .unwrap()
        .into_iter()
        .map(|r| r["cache_key"].as_str().unwrap().to_string())
        .collect();
    assert!(!remaining.contains(&"k|0".to_string()), "oldest gone");
    assert!(remaining.contains(&"k|9".to_string()), "newest kept");
}

#[test]
fn cache_get_roundtrips_fields() {
    let h = handle("roundtrip.db");
    assert!(cache_get(&h, "none|1").unwrap().is_none());
    cache_put(&h, "a|1", "S", "F", "D", "B", true).unwrap();
    let v = cache_get(&h, "a|1").unwrap().unwrap();
    assert_eq!(v["subject"], json!("S"));
    assert_eq!(v["sender"], json!("F"));
    assert_eq!(v["date_iso"], json!("D"));
    assert_eq!(v["body_truncated"], true);
}

#[test]
fn prune_on_empty_table_is_a_noop() {
    let h = handle("prune-empty.db");
    prune_bodies(&h, 100).unwrap(); // must not panic or error
    let n: i64 = h
        .query("SELECT COUNT(*) AS n FROM mail_bodies", &[])
        .unwrap()
        .remove(0)["n"]
        .as_i64()
        .unwrap();
    assert_eq!(n, 0);
}

#[test]
fn recache_updates_fetched_at_for_lru_order() {
    let h = handle("recache.db");
    cache_put(&h, "a|1", "s", "f", "d", "old", false).unwrap();
    h.query(
        "UPDATE mail_bodies SET fetched_at = 1 WHERE cache_key = 'a|1'",
        &[],
    )
    .unwrap();
    // Re-read later → same key rewritten with a fresh timestamp so eviction
    // order reflects actual access, not first-read.
    cache_put(&h, "a|1", "s", "f", "d", "newer", false).unwrap();
    let v = cache_get(&h, "a|1").unwrap().unwrap();
    let ts = h
        .query(
            "SELECT fetched_at FROM mail_bodies WHERE cache_key = 'a|1'",
            &[],
        )
        .unwrap()
        .remove(0)["fetched_at"]
        .as_i64()
        .unwrap();
    assert!(ts > 1, "fetched_at refreshed on rewrite");
    assert_eq!(v["body"], json!("newer"));
}
