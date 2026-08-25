//! Contract tests for index-mode `mail_search`: SQL-backed filtering,
//! grouping, and pagination over the synced corpus.

use mcp_macos::mail::MailGroupBy;
use mcp_macos::mail_index::{IndexQuery, search_index};
use personai_core::index::IndexHandle;
use serde_json::Value;

fn seeded(name: &str) -> IndexHandle {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    std::mem::forget(dir);
    let h = IndexHandle::open(path, mcp_macos::mail_index::MAIL_MIGRATIONS).unwrap();
    // Two folders; senders with multiple threads; staggered dates.
    h.upsert(
        "mail_messages",
        &[
            "account",
            "mailbox",
            "apple_id",
            "fingerprint",
            "subject",
            "sender",
            "date_received",
            "fetched_at",
        ],
        &["account", "mailbox", "apple_id"],
        &[
            row(
                "Exchange",
                "Apps",
                "301",
                "IBM <talent@ibm.com>",
                "Action Required: IBM Assessment",
                "2026-08-20T10:00:00.000Z",
            ),
            row(
                "Exchange",
                "Apps",
                "302",
                "Meta <careers@meta.com>",
                "Next steps with your application",
                "2026-08-21T10:00:00.000Z",
            ),
            row(
                "Exchange",
                "Apps",
                "303",
                "IBM <talent@ibm.com>",
                "Re: IBM Assessment reminder",
                "2026-08-23T10:00:00.000Z",
            ),
            row(
                "Google",
                "INBOX",
                "304",
                "noreply@amazon.jobs",
                "Update on your application",
                "2026-08-19T10:00:00.000Z",
            ),
            row(
                "Google",
                "INBOX",
                "305",
                "noreply@amazon.jobs",
                "You are invited to interview",
                "2026-08-24T10:00:00.000Z",
            ),
            row(
                "Google",
                "RIP",
                "306",
                "reject@corp.com",
                "Unfortunately... rejection",
                "2026-08-18T10:00:00.000Z",
            ),
        ],
    )
    .unwrap();
    for (k, c) in [("Exchange/Apps", 3), ("Google/INBOX", 2), ("Google/RIP", 1)] {
        h.set_watermark(
            "mail",
            k,
            "2026-08-24T10:00:00.000Z",
            &serde_json::json!({"count": c}),
        )
        .unwrap();
    }
    h
}

#[allow(clippy::too_many_arguments)]
fn row(account: &str, mailbox: &str, id: &str, sender: &str, subject: &str, date: &str) -> Value {
    serde_json::json!({
        "account": account, "mailbox": mailbox, "apple_id": id,
        "fingerprint": mcp_macos::mail_index::fingerprint(sender, subject, date),
        "subject": subject, "sender": sender, "date_received": date,
        "fetched_at": 1_700_000_000i64,
    })
}

fn q<'a>(
    terms: &'a [String],
    pairs: Option<&'a [(String, String)]>,
    group: Option<MailGroupBy>,
    limit: u32,
    offset: u32,
) -> IndexQuery<'a> {
    IndexQuery {
        terms,
        pairs,
        since: None,
        until: None,
        limit,
        offset,
        group,
    }
}

#[test]
fn rows_filter_by_term_folder_and_paginate() {
    let h = seeded("rows.db");
    let terms = vec!["assessment".to_string()];
    let pairs = vec![("Exchange".to_string(), "Apps".to_string())];
    let out = search_index(&h, &q(&terms, Some(&pairs), None, 20, 0)).unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["total"], 2, "term hits both IBM rows");
    assert_eq!(v["offset"], 0);
    assert_eq!(v["limit"], 20);
    assert_eq!(v["data_as_of"], "2026-08-24T10:00:00.000Z");
    let results = v["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    // Newest first.
    assert_eq!(results[0]["id"], "303");
    assert_eq!(results[1]["id"], "301");
    assert_eq!(results[0]["folder"], "Exchange/Apps");
    assert!(
        results[0].get("snippet").is_none(),
        "snippets stay off in index mode"
    );
}

#[test]
fn pagination_slices_results() {
    let h = seeded("page.db");
    let empty: Vec<String> = vec![];
    let out = search_index(&h, &q(&empty, None, None, 2, 2)).unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["total"], 6, "no folder filter = whole corpus");
    let ids: Vec<&str> = v["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    // Sorted desc: 305,303,302,301,304,306 — page at offset 2 size 2.
    assert_eq!(ids, ["302", "301"]);
}

#[test]
fn groups_collapse_senders_like_live_mode() {
    let h = seeded("groups.db");
    let empty: Vec<String> = vec![];
    let out = search_index(&h, &q(&empty, None, Some(MailGroupBy::Sender), 20, 0)).unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["total"], 6);
    assert_eq!(v["total_groups"], 4);
    let groups = v["groups"].as_array().unwrap();
    // amazon.jobs has 2 messages — top group by count then recency.
    assert_eq!(groups[0]["key"], "noreply@amazon.jobs");
    assert_eq!(groups[0]["count"], 2);
    assert_eq!(groups[0]["latest_id"], "305");
    assert_eq!(groups[0]["folders"][0], "Google/INBOX");
    assert!(groups[0]["sample_subjects"].as_array().unwrap().len() == 2);
    // Single-message groups follow by last desc: 306 (08-18) is oldest → last.
    let keys: Vec<&str> = groups.iter().map(|g| g["key"].as_str().unwrap()).collect();
    assert_eq!(keys.last().unwrap(), &"reject@corp.com");
    // THE multi-posting signal: amazon sent 2 messages with 2 DISTINCT
    // normalized subjects ⇒ distinct_subjects tells the agent how many
    // threads exist behind one sender, even though samples cap at 4.
    assert_eq!(groups[0]["distinct_subjects"], 2);
}

#[test]
fn subject_groups_omit_distinct_signal() {
    let h = seeded("gsub.db");
    let empty: Vec<String> = vec![];
    let out = search_index(&h, &q(&empty, None, Some(MailGroupBy::Subject), 20, 0)).unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    let g = v["groups"].as_array().unwrap();
    assert!(
        g.iter().all(|x| x.get("distinct_subjects").is_none()),
        "distinct_subjects is a sender-mode-only field"
    );
}

#[test]
fn groups_respect_folder_scope() {
    let h = seeded("gscope.db");
    let empty: Vec<String> = vec![];
    let pairs = vec![
        ("Exchange".to_string(), "Apps".to_string()),
        ("Google".to_string(), "RIP".to_string()),
    ];
    let out = search_index(
        &h,
        &q(&empty, Some(&pairs), Some(MailGroupBy::Sender), 20, 0),
    )
    .unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["total"], 4);
    let keys: Vec<&str> = v["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys.len(), 3);
    assert!(keys.contains(&"ibm.com") || keys.contains(&"talent@ibm.com"));
    assert!(
        !keys.contains(&"noreply@amazon.jobs"),
        "INBOX excluded from scope"
    );
}

#[test]
fn empty_index_returns_zero_totals_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let h = IndexHandle::open(
        dir.path().join("empty.db"),
        mcp_macos::mail_index::MAIL_MIGRATIONS,
    )
    .unwrap();
    std::mem::forget(dir);
    let empty: Vec<String> = vec![];
    let out = search_index(&h, &q(&empty, None, None, 20, 0)).unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["total"], 0);
    assert_eq!(v["results"].as_array().unwrap().len(), 0);
    assert_eq!(
        v["data_as_of"], "",
        "never-synced index reports empty freshness"
    );
}

#[test]
fn until_window_is_exclusive_and_terms_case_insensitive() {
    let h = seeded("window.db");
    let terms = vec!["ASSESSMENT".to_string()];
    let out = search_index(
        &h,
        &IndexQuery {
            terms: &terms,
            pairs: None,
            since: None,
            until: Some("2026-08-21T00:00:00.000Z"),
            limit: 20,
            offset: 0,
            group: None,
        },
    )
    .unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    // Uppercase term matches lowercase-stored subject; until excludes 08-21+.
    assert_eq!(v["total"], 1);
    assert_eq!(v["results"][0]["id"], "301");
}

#[test]
fn offset_past_total_yields_empty_page_with_true_total() {
    let h = seeded("overrun.db");
    let empty: Vec<String> = vec![];
    let out = search_index(&h, &q(&empty, None, None, 20, 500)).unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["total"], 6);
    assert_eq!(v["results"].as_array().unwrap().len(), 0);
}
