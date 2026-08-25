//! Shared index schema composition.
//!
//! Mail and Messages (and future surfaces) share ONE `index.db`. SQLite's
//! `PRAGMA user_version` is per-file, so every opener must pass the SAME
//! ordered migration list — composed here from each surface's steps. A
//! binary that knows fewer migrations than the file's stamp is rejected
//! (`IndexError::FutureSchema`) instead of misreading newer schema.

use crate::mail_index;

/// v1: mail corpus tables (see `mail_index::MAIL_V1` for the DDL).
pub const MAIL_V1: &str = mail_index::MAIL_V1;

/// v2 placeholder — replaced by the messages surface DDL in its own cycle.
/// v2: Messages corpus mirror of chat.db. `rowid_src` is chat.db's
/// monotone `message.ROWID` — the natural incremental watermark. FTS5
/// external-content mirror over `text`, maintained by triggers.
pub const MESSAGES_V1: &str = r#"
CREATE TABLE IF NOT EXISTS msg_messages(
  rowid_src INTEGER NOT NULL,
  chat_identifier TEXT NOT NULL DEFAULT '',
  is_from_me INTEGER NOT NULL DEFAULT 0,
  sender TEXT NOT NULL DEFAULT '',
  text TEXT NOT NULL DEFAULT '',
  date_iso TEXT NOT NULL,
  fetched_at INTEGER NOT NULL,
  PRIMARY KEY(rowid_src));
CREATE INDEX IF NOT EXISTS idx_msg_chat ON msg_messages(chat_identifier);
CREATE INDEX IF NOT EXISTS idx_msg_date ON msg_messages(date_iso);
CREATE VIRTUAL TABLE IF NOT EXISTS msg_fts USING fts5(
  text, content='msg_messages', content_rowid='rowid_src');
CREATE TRIGGER IF NOT EXISTS msg_ai AFTER INSERT ON msg_messages BEGIN
  INSERT INTO msg_fts(rowid, text) VALUES(new.rowid_src, new.text); END;
CREATE TRIGGER IF NOT EXISTS msg_ad AFTER DELETE ON msg_messages BEGIN
  INSERT INTO msg_fts(msg_fts, rowid, text) VALUES('delete', old.rowid_src, old.text); END;
CREATE TRIGGER IF NOT EXISTS msg_au AFTER UPDATE ON msg_messages BEGIN
  INSERT INTO msg_fts(msg_fts, rowid, text) VALUES('delete', old.rowid_src, old.text);
  INSERT INTO msg_fts(rowid, text) VALUES(new.rowid_src, new.text); END;
"#;

/// The canonical, ordered list EVERY `IndexHandle::open` on `index.db`
/// must pass.
pub const INDEX_MIGRATIONS: &[&str] = &[MAIL_V1, MESSAGES_V1];
