//! Mail corpus index: schema, fingerprints, and the sync driver that pulls
//! message metadata into `index.db` so searches stop re-transferring the
//! corpus through Apple Events.
//!
//! Split of responsibilities: [`personai_core::index`] owns generic
//! storage primitives; this module owns the mail schema and the JXA fetch
//! scripts. One osascript run per folder keeps every folder commit
//! independent — a sweep interrupted by the transport timeout resumes on
//! the next call with no partial-partition states.

use crate::mail::{MailGroupBy, MailTargets};
use crate::util::js_str;
use personai_core::index::{IndexError, IndexHandle};
use personai_core::macos::{AppleError, AppleTransport, run_jxa_json};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// v1 of the mail surface's tables. `mailbox_key` is a virtual generated
/// column (`account || '/' || mailbox`) so partition replace and scope
/// filters key off one indexed string. FTS5 external-content mirror is
/// maintained by triggers — the engine never learns about it.
pub const MAIL_MIGRATIONS: &[&str] = &[r#"
CREATE TABLE IF NOT EXISTS mail_messages(
  account TEXT NOT NULL, mailbox TEXT NOT NULL, apple_id TEXT NOT NULL,
  fingerprint TEXT NOT NULL,
  subject TEXT NOT NULL DEFAULT '', sender TEXT NOT NULL DEFAULT '',
  date_received TEXT NOT NULL, fetched_at INTEGER NOT NULL,
  mailbox_key TEXT GENERATED ALWAYS AS (account || '/' || mailbox) VIRTUAL,
  PRIMARY KEY(account, mailbox, apple_id));
CREATE INDEX IF NOT EXISTS idx_mail_date ON mail_messages(date_received);
CREATE INDEX IF NOT EXISTS idx_mail_key ON mail_messages(mailbox_key);
CREATE VIRTUAL TABLE IF NOT EXISTS mail_fts USING fts5(
  subject, sender, content='mail_messages', content_rowid='rowid');
CREATE TRIGGER IF NOT EXISTS mail_ai AFTER INSERT ON mail_messages BEGIN
  INSERT INTO mail_fts(rowid, subject, sender)
    VALUES(new.rowid, new.subject, new.sender); END;
CREATE TRIGGER IF NOT EXISTS mail_ad AFTER DELETE ON mail_messages BEGIN
  INSERT INTO mail_fts(mail_fts, rowid, subject, sender)
    VALUES('delete', old.rowid, old.subject, old.sender); END;
CREATE TRIGGER IF NOT EXISTS mail_au AFTER UPDATE ON mail_messages BEGIN
  INSERT INTO mail_fts(mail_fts, rowid, subject, sender)
    VALUES('delete', old.rowid, old.subject, old.sender);
  INSERT INTO mail_fts(rowid, subject, sender)
    VALUES(new.rowid, new.subject, new.sender); END;
CREATE TABLE IF NOT EXISTS mail_bodies(
  cache_key TEXT PRIMARY KEY, subject TEXT, sender TEXT, date_iso TEXT,
  body TEXT NOT NULL, body_truncated INTEGER NOT NULL DEFAULT 0,
  fetched_at INTEGER NOT NULL);
"#];

const SOURCE: &str = "mail";

#[derive(Debug, Clone, Copy, Default)]
pub struct FolderSyncStats {
    pub scanned: usize,
    pub new: usize,
    pub updated: usize,
    pub mismatches: usize,
}

impl FolderSyncStats {
    fn to_json(self) -> Value {
        json!({
            "scanned": self.scanned, "new": self.new,
            "updated": self.updated, "mismatches": self.mismatches,
        })
    }
}

fn apple(e: impl std::fmt::Display) -> AppleError {
    AppleError::Transport(e.to_string())
}

/// Stable content hash of `(sender, subject, date_received)`: detects
/// Apple-id reuse when a stored id now points at different content.
pub fn fingerprint(sender: &str, subject: &str, date_received: &str) -> String {
    let mut hasher = Sha256::new();
    for part in [sender, subject, date_received] {
        hasher.update(part.as_bytes());
        hasher.update([0u8]);
    }
    hasher.finalize()[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// JXA builder for one folder's bulk metadata fetch. Same array-guarded
/// account/mailbox resolution as `search_multi_expr`; four bulk Apple
/// Events sliced to `scan`; optional watermark floor applied in JS (the
/// floor is the stored watermark minus a 1h clock-skew buffer).
pub fn sync_expr(account: &str, mailbox: &str, since_iso: Option<&str>, scan: u32) -> String {
    let since_clause = match since_iso {
        Some(iso) => format!("const SINCE_MS = Date.parse({}) - 3600000;", js_str(iso)),
        None => "const SINCE_MS = null;".to_string(),
    };
    format!(
        r#"(() => {{
  const M = Application('Mail');
  {since_clause}
  let box;
  try {{
    const accs = M.accounts.whose({{name: {}}})();
    const boxes = accs.length ? accs[0].mailboxes.whose({{name: {}}})() : [];
    if (!boxes.length) return {{rows: [], scanned: 0}};
    box = boxes[0];
  }} catch (e) {{ return {{rows: [], scanned: 0}}; }}
  const scan = Math.min(box.messages.length, {scan});
  const ids = box.messages.id().slice(0, scan);
  const subjects = box.messages.subject().slice(0, scan);
  const senders = box.messages.sender().slice(0, scan);
  const dates = box.messages.dateReceived().slice(0, scan);
  const rows = [];
  for (let i = 0; i < ids.length; i++) {{
    if (SINCE_MS !== null && dates[i] && dates[i].getTime() < SINCE_MS) continue;
    rows.push({{
      i: String(ids[i]),
      s: subjects[i] == null ? '' : String(subjects[i]),
      f: senders[i] == null ? '' : String(senders[i]),
      d: dates[i] ? dates[i].toISOString() : ''
    }});
  }}
  return {{rows: rows, scanned: ids.length}};
}})()"#,
        js_str(account),
        js_str(mailbox),
    )
}

struct Row {
    apple_id: String,
    fingerprint: String,
    subject: String,
    sender: String,
    date_received: String,
}

fn parse_rows(v: &Value) -> Vec<Row> {
    v.get("rows")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|r| {
                    let s = r["s"].as_str().unwrap_or_default();
                    let f = r["f"].as_str().unwrap_or_default();
                    let d = r["d"].as_str().unwrap_or_default();
                    Row {
                        apple_id: r["i"].as_str().unwrap_or_default().to_string(),
                        fingerprint: fingerprint(f, s, d),
                        subject: s.to_string(),
                        sender: f.to_string(),
                        date_received: d.to_string(),
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

fn to_storage_rows(rows: &[Row], fetched_at: i64) -> Vec<Value> {
    rows.iter()
        .map(|r| {
            json!({
                "apple_id": r.apple_id, "fingerprint": r.fingerprint,
                "subject": r.subject, "sender": r.sender,
                "date_received": r.date_received, "fetched_at": fetched_at,
            })
        })
        .collect()
}

async fn fetch_rows<T: AppleTransport>(
    t: &mut T,
    account: &str,
    mailbox: &str,
    since_iso: Option<&str>,
    scan: u32,
) -> Result<(Vec<Row>, usize), AppleError> {
    let expr = sync_expr(account, mailbox, since_iso, scan);
    let v = run_jxa_json(t, &expr).await?;
    let rows = parse_rows(&v);
    // Real scripts always report `scanned`; fall back to the row count so
    // degraded payloads still produce honest stats.
    let scanned = v
        .get("scanned")
        .and_then(Value::as_u64)
        .map(|s| s as usize)
        .unwrap_or(rows.len());
    Ok((rows, scanned))
}

const STORAGE_COLS: &[&str] = &[
    "account",
    "mailbox",
    "apple_id",
    "fingerprint",
    "subject",
    "sender",
    "date_received",
    "fetched_at",
];

/// Sync one folder into the index. `full` (or first sight) replaces the
/// whole partition; otherwise a delta pass upserts rows at/after the
/// watermark and counts fingerprint mismatches for existing ids.
pub async fn sync_folder<T: AppleTransport>(
    t: &mut T,
    h: &IndexHandle,
    account: &str,
    mailbox: &str,
    full: bool,
    scan: u32,
) -> Result<FolderSyncStats, AppleError> {
    let key = format!("{account}/{mailbox}");
    let stored_wm = h.watermark(SOURCE, &key).map_err(apple)?;
    let have_watermark = stored_wm.as_ref().is_some_and(|(wm, _, _)| !wm.is_empty());
    let delta_since: Option<String> = if full || !have_watermark {
        None
    } else {
        stored_wm.as_ref().map(|(wm, _, _)| wm.clone())
    };

    let (rows, scanned) = fetch_rows(t, account, mailbox, delta_since.as_deref(), scan).await?;

    let mut stats = FolderSyncStats {
        scanned,
        ..Default::default()
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    if full || !have_watermark {
        let mut storage = to_storage_rows(&rows, now);
        for r in storage.iter_mut() {
            r["account"] = json!(account);
            r["mailbox"] = json!(mailbox);
        }
        stats.new = h
            .replace_partition(
                "mail_messages",
                "mailbox_key",
                &key,
                STORAGE_COLS,
                &["account", "mailbox", "apple_id"],
                &storage,
            )
            .map_err(apple)?;
    } else {
        let existing: Vec<Value> = h
            .query(
                "SELECT apple_id, fingerprint FROM mail_messages WHERE mailbox_key = ?1",
                &[json!(key)],
            )
            .map_err(apple)?;
        let known: std::collections::HashMap<String, String> = existing
            .into_iter()
            .map(|r| {
                (
                    r["apple_id"].as_str().unwrap_or_default().to_string(),
                    r["fingerprint"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        for r in &rows {
            match known.get(&r.apple_id) {
                None => stats.new += 1,
                Some(fp) => {
                    stats.updated += 1;
                    if fp != &r.fingerprint {
                        stats.mismatches += 1;
                    }
                }
            }
        }
        let mut storage = to_storage_rows(&rows, now);
        for r in storage.iter_mut() {
            r["account"] = json!(account);
            r["mailbox"] = json!(mailbox);
        }
        h.upsert(
            "mail_messages",
            STORAGE_COLS,
            &["account", "mailbox", "apple_id"],
            &storage,
        )
        .map_err(apple)?;
    }

    // Watermark = newest date seen (stored or this batch), never regresses.
    let batch_max = rows
        .iter()
        .filter_map(|r| (!r.date_received.is_empty()).then_some(r.date_received.clone()))
        .max();
    let old_wm = stored_wm.map(|(w, _, _)| w).unwrap_or_default();
    let wm = match batch_max {
        Some(b) if b > old_wm => b,
        _ => old_wm,
    };
    let count: i64 = h
        .query(
            "SELECT COUNT(*) AS n FROM mail_messages WHERE mailbox_key = ?1",
            &[json!(key)],
        )
        .map_err(apple)?
        .remove(0)["n"]
        .as_i64()
        .unwrap_or(0);
    h.set_watermark(SOURCE, &key, &wm, &json!({"count": count}))
        .map_err(apple)?;

    Ok(stats)
}

/// Sweep every target folder independently (per-folder commit ⇒ resumable).
/// Unified mode is rejected: it has no stable per-folder identity to
/// partition by — callers expand concrete folders instead.
pub async fn sync_targets<T: AppleTransport>(
    t: &mut T,
    h: &IndexHandle,
    targets: &MailTargets,
    full: bool,
    scan: u32,
) -> Result<String, AppleError> {
    let pairs = match targets {
        MailTargets::Unified => {
            return Err(AppleError::Transport(
                "index sync needs concrete folders — pass folders:[\"Account/Mailbox\", …] \
                 or folders:[\"*\"]; the unified inbox spans accounts and cannot be \
                 partitioned"
                    .into(),
            ));
        }
        MailTargets::Folders(pairs) => pairs.clone(),
    };
    let t0 = std::time::Instant::now();
    let mut per_folder = serde_json::Map::new();
    for (account, mailbox) in pairs {
        let key = format!("{account}/{mailbox}");
        let stats = sync_folder(t, h, &account, &mailbox, full, scan).await?;
        per_folder.insert(key, stats.to_json());
    }
    // data_as_of = max watermark across swept scopes.
    let mut data_as_of = String::new();
    for key in per_folder.keys() {
        if let Some((wm, _, _)) = h.watermark(SOURCE, key).map_err(apple)?
            && wm > data_as_of
        {
            data_as_of = wm;
        }
    }
    Ok(json!({
        "synced_per_folder": Value::Object(per_folder),
        "data_as_of": data_as_of,
        "duration_ms": t0.elapsed().as_millis() as u64,
    })
    .to_string())
}

/// Query parameters for index-mode search (mirrors `mail_search` semantics).
pub struct IndexQuery<'a> {
    pub terms: &'a [String],
    /// `None` = whole synced corpus (unified equivalent).
    pub pairs: Option<&'a [(String, String)]>,
    pub since: Option<&'a str>,
    pub until: Option<&'a str>,
    pub limit: u32,
    pub offset: u32,
    pub group: Option<MailGroupBy>,
}

/// SQL-backed equivalent of `search_multi_expr`: term/folder/date filters,
/// pagination, and live-mode-compatible sender/subject grouping execute
/// against `index.db` instead of Apple Events. ISO-8601 UTC dates compare
/// correctly as strings, so windows and ordering need no date parsing.
pub fn search_index(h: &IndexHandle, q: &IndexQuery) -> Result<String, IndexError> {
    const SEL: &str =
        "SELECT apple_id, subject, sender, date_received, mailbox_key FROM mail_messages";

    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<Value> = Vec::new();
    if let Some(pairs) = q.pairs.filter(|p| !p.is_empty()) {
        let keys: Vec<String> = pairs
            .iter()
            .map(|(a, b)| format!("{a}/{b}"))
            .collect::<Vec<_>>();
        let ph = vec!["?"; keys.len()].join(", ");
        clauses.push(format!("mailbox_key IN ({ph})"));
        params.extend(keys.into_iter().map(|k| json!(k)));
    }
    if let Some(s) = q.since.filter(|s| !s.is_empty()) {
        clauses.push("date_received >= ?".into());
        params.push(json!(s));
    }
    if let Some(u) = q.until.filter(|u| !u.is_empty()) {
        clauses.push("date_received < ?".into());
        params.push(json!(u));
    }
    for t in q.terms {
        let tl = t.trim().to_lowercase();
        if tl.is_empty() {
            continue;
        }
        clauses.push("(instr(lower(subject), ?) > 0 OR instr(lower(sender), ?) > 0)".into());
        params.push(json!(tl));
        params.push(json!(tl));
    }
    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };

    // Freshness marker: max watermark across the queried scopes.
    let scope_keys: Vec<String> = match q.pairs {
        Some(pairs) => pairs.iter().map(|(a, b)| format!("{a}/{b}")).collect(),
        None => h
            .query(
                "SELECT scope_key FROM sync_state WHERE source = 'mail'",
                &[],
            )?
            .into_iter()
            .filter_map(|r| r["scope_key"].as_str().map(str::to_string))
            .collect(),
    };
    let mut data_as_of = String::new();
    for key in &scope_keys {
        if let Some((wm, _, _)) = h.watermark("mail", key)?
            && wm > data_as_of
        {
            data_as_of = wm;
        }
    }

    let group = q.group.clone();
    match group {
        Some(group) => {
            let all = h.query(
                &format!("{SEL} {where_sql} ORDER BY date_received DESC"),
                &params,
            )?;
            let total = all.len();

            let norm_sub = |s: &str| -> String {
                let mut t = s.trim().to_lowercase();
                loop {
                    let stripped = t
                        .trim_start()
                        .trim_start_matches("re:")
                        .trim_start()
                        .trim_start_matches("fwd:")
                        .trim_start()
                        .trim_start_matches("fw:");
                    if stripped.len() == t.len() {
                        break;
                    }
                    t = stripped.to_string();
                }
                t.split_whitespace().collect::<Vec<_>>().join(" ")
            };
            let addr_of = |f: &str| -> String {
                let f = f.trim();
                match (f.find('<'), f.find('>')) {
                    (Some(a), Some(b)) if b > a => f[a + 1..b].trim().to_lowercase(),
                    _ => f.to_lowercase(),
                }
            };
            let name_of = |f: &str| -> String {
                let a = addr_of(f);
                let base = f.split('<').next().unwrap_or("").trim();
                let base = base.trim_matches('"');
                if base.is_empty() { a } else { base.to_string() }
            };

            struct G {
                name: String,
                count: usize,
                first: String,
                last: String,
                latest_id: String,
                latest_ids: Vec<String>,
                samples: Vec<String>,
                folders: Vec<String>,
            }
            let mut map: std::collections::HashMap<String, G> = std::collections::HashMap::new();
            for e in &all {
                let from = e["sender"].as_str().unwrap_or_default();
                let subject = e["subject"].as_str().unwrap_or_default();
                let date = e["date_received"].as_str().unwrap_or_default();
                let id = e["apple_id"].as_str().unwrap_or_default();
                let folder = e["mailbox_key"].as_str().unwrap_or_default();
                let key = match group {
                    MailGroupBy::Sender => addr_of(from),
                    MailGroupBy::Subject => norm_sub(subject),
                };
                let g = map.entry(key.clone()).or_insert_with(|| G {
                    name: name_of(from),
                    count: 0,
                    first: date.to_string(),
                    last: String::new(),
                    latest_id: String::new(),
                    latest_ids: Vec::new(),
                    samples: Vec::new(),
                    folders: Vec::new(),
                });
                g.count += 1;
                if g.first.as_str() > date {
                    g.first = date.to_string();
                }
                if g.last.as_str() < date {
                    g.last = date.to_string();
                    g.latest_id = id.to_string();
                }
                if g.latest_ids.len() < 3 {
                    g.latest_ids.push(id.to_string());
                }
                let ns = norm_sub(subject);
                if g.samples.len() < 4 && !g.samples.iter().any(|x| norm_sub(x) == ns) {
                    g.samples.push(subject.to_string());
                }
                if g.folders.len() < 3 && !g.folders.iter().any(|x| x == folder) {
                    g.folders.push(folder.to_string());
                }
            }
            let mut groups: Vec<(String, G)> = map.into_iter().collect();
            groups.sort_by(|a, b| {
                b.1.count
                    .cmp(&a.1.count)
                    .then_with(|| b.1.last.cmp(&a.1.last))
            });
            let total_groups = groups.len();
            let start = (q.offset as usize).min(total_groups);
            let end = ((q.offset as usize) + q.limit as usize).min(total_groups);
            let page: Vec<Value> = groups[start..end]
                .iter()
                .map(|(key, g)| {
                    json!({
                        "key": key,
                        "name": if matches!(group, MailGroupBy::Sender) { g.name.clone() } else { key.clone() },
                        "count": g.count,
                        "first": g.first,
                        "last": g.last,
                        "latest_id": g.latest_id,
                        "latest_ids": g.latest_ids,
                        "sample_subjects": g.samples,
                        "folders": g.folders,
                    })
                })
                .collect();
            Ok(json!({
                "total": total,
                "total_groups": total_groups,
                "groups": page,
                "data_as_of": data_as_of,
            })
            .to_string())
        }
        None => {
            let total = h
                .query(
                    &format!("SELECT COUNT(*) AS n FROM mail_messages {where_sql}"),
                    &params,
                )?
                .remove(0)["n"]
                .as_i64()
                .unwrap_or(0);
            let mut page_params = params.clone();
            page_params.push(json!(q.limit));
            page_params.push(json!(q.offset));
            let rows = h.query(
                &format!("{SEL} {where_sql} ORDER BY date_received DESC LIMIT ? OFFSET ?"),
                &page_params,
            )?;
            let results: Vec<Value> = rows
                .iter()
                .map(|r| {
                    json!({
                        "id": r["apple_id"],
                        "subject": r["subject"],
                        "from": r["sender"],
                        "date": r["date_received"],
                        "folder": r["mailbox_key"],
                    })
                })
                .collect();
            Ok(json!({
                "total": total,
                "offset": q.offset,
                "limit": q.limit,
                "results": results,
                "data_as_of": data_as_of,
            })
            .to_string())
        }
    }
}

/// Fetch a cached body envelope (`subject`,`sender`,`date_iso`,`body`,
/// `body_truncated`) or None.
pub fn cache_get(h: &IndexHandle, key: &str) -> Result<Option<Value>, IndexError> {
    let mut rows = h.query(
        "SELECT subject, sender, date_iso, body, body_truncated \
         FROM mail_bodies WHERE cache_key = ?1",
        &[json!(key)],
    )?;
    match rows.pop() {
        Some(mut v) => {
            if let Some(o) = v.as_object_mut() {
                let flag = o.get("body_truncated").and_then(Value::as_i64).unwrap_or(0) != 0;
                o.insert("body_truncated".into(), json!(flag));
            }
            Ok(Some(v))
        }
        None => Ok(None),
    }
}

/// Store a body under `key` ("Account/Mailbox|id" or "?|id").
#[allow(clippy::too_many_arguments)]
pub fn cache_put(
    h: &IndexHandle,
    key: &str,
    subject: &str,
    sender: &str,
    date_iso: &str,
    body: &str,
    truncated: bool,
) -> Result<(), IndexError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    h.upsert(
        "mail_bodies",
        &[
            "cache_key",
            "subject",
            "sender",
            "date_iso",
            "body",
            "body_truncated",
            "fetched_at",
        ],
        &["cache_key"],
        &[json!({
            "cache_key": key, "subject": subject, "sender": sender,
            "date_iso": date_iso, "body": body,
            "body_truncated": truncated as i64, "fetched_at": now,
        })],
    )?;
    Ok(())
}

/// Evict oldest bodies until total stored bytes fit `cap_bytes`.
pub fn prune_bodies(h: &IndexHandle, cap_bytes: u64) -> Result<(), IndexError> {
    // Rare maintenance path: evict strictly oldest-first, one row per pass,
    // until the stored total fits. Exact — never overshoots the cap.
    loop {
        let total = h
            .query(
                "SELECT COALESCE(SUM(LENGTH(body)), 0) AS n FROM mail_bodies",
                &[],
            )?
            .remove(0)["n"]
            .as_i64()
            .unwrap_or(0);
        if total <= cap_bytes as i64 {
            return Ok(());
        }
        h.query(
            "DELETE FROM mail_bodies WHERE cache_key = (
               SELECT cache_key FROM mail_bodies ORDER BY fetched_at ASC LIMIT 1)",
            &[],
        )?;
    }
}
