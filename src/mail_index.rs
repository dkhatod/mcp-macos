//! Mail corpus index: schema, fingerprints, and the sync driver that pulls
//! message metadata into `index.db` so searches stop re-transferring the
//! corpus through Apple Events.
//!
//! Split of responsibilities: [`personai_core::index`] owns generic
//! storage primitives; this module owns the mail schema and the JXA fetch
//! scripts. One osascript run per folder keeps every folder commit
//! independent — a sweep interrupted by the transport timeout resumes on
//! the next call with no partial-partition states.

use crate::mail::MailTargets;
use crate::util::js_str;
use personai_core::index::IndexHandle;
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
