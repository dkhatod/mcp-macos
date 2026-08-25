//! iMessage/SMS tools.
//!
//! `read` goes through `sqlite3` on the chat.db (the only reliable history
//! source; needs Full Disk Access), executed inside the JXA shell so it
//! stays behind the same transport + error taxonomy as every other group.
//! `send` is soft-gated.
//!
//! Context discipline: `read` is paginated (`total`/`offset`/`limit`) and
//! returns per-message metadata only.

use personai_core::macos::{AppleError, AppleTransport, run_jxa_json, wrap_jxa};
use personai_core::safety::{GateOutcome, SoftGate};
use serde_json::json;
use std::time::Duration;

use crate::util::{js_str, sql_shell_lines_js};
use crate::{DEFAULT_LIMIT, MAX_LIMIT};

/// iMessage tool group over any transport.
pub struct MessagesToolset<T: AppleTransport> {
    pub transport: T,
    /// Soft gate for sends (token store under the state dir).
    pub gate: Option<SoftGate>,
}

impl<T: AppleTransport> MessagesToolset<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            gate: None,
        }
    }

    /// Production constructor: gate backed by `token_store`.
    pub fn with_gate(transport: T, token_store: std::path::PathBuf) -> Result<Self, AppleError> {
        Ok(Self {
            transport,
            gate: Some(
                SoftGate::new(token_store)
                    .map_err(|e| AppleError::Transport(format!("gate unavailable: {e}")))?,
            ),
        })
    }

    /// Reads recent messages, newest first. `chat` matches a chat id,
    /// display name, or participant handle.
    pub async fn read(
        &mut self,
        chat: Option<String>,
        limit: Option<u32>,
        offset: u32,
    ) -> Result<String, AppleError> {
        let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
        let expr = read_expr(chat.as_deref(), limit, offset);
        match run_jxa_json(&mut self.transport, &expr).await {
            Ok(v) => Ok(crate::util::sanitize_json_text(
                &crate::util::unwrap_string_payload(v)?.to_string(),
            )),
            Err(e @ AppleError::Parse(_)) => {
                // Live-quirk diagnostic: dump the raw osascript stdout so
                // the user can see what broke the single-line envelope.
                // Zero-cost unless MCP_MACOS_DEBUG_RAW=1 is exported.
                if matches!(std::env::var("MCP_MACOS_DEBUG_RAW").as_deref(), Ok("1")) {
                    let raw = self
                        .transport
                        .run(&wrap_jxa(&expr), Duration::from_secs(30))
                        .await;
                    let raw = raw.unwrap_or_default();
                    let chars: Vec<char> = raw.chars().collect();
                    let head: String = chars.iter().take(500).collect();
                    let tail: String = chars
                        .iter()
                        .rev()
                        .take(300)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    eprintln!(
                        "[MCP_MACOS_DEBUG_RAW] read failed ({e}); raw stdout length={}\nHEAD(500): {head}\nTAIL(300): {tail}",
                        chars.len()
                    );
                }
                Err(e)
            }
            Err(e) => Err(e),
        }
    }

    /// Lists chats (identifier, display name, service, sample handle,
    /// message count, last activity), most recent first. Auto-tier.
    pub async fn chats(&mut self, limit: Option<u32>, offset: u32) -> Result<String, AppleError> {
        let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
        let v = run_jxa_json(&mut self.transport, &chats_expr(limit, offset)).await?;
        Ok(crate::util::sanitize_json_text(
            &crate::util::unwrap_string_payload(v)?.to_string(),
        ))
    }

    /// Mirror chat.db into `index.db` via the shared engine handle.
    pub async fn sync_index(
        &mut self,
        h: &personai_core::index::IndexHandle,
        full: bool,
    ) -> Result<String, AppleError> {
        crate::messages_index::sync_messages(
            &mut self.transport,
            h,
            full,
            crate::messages_index::BATCH_ROWS,
        )
        .await
    }

    /// Per-chat unread counts, heaviest first — triage/digest input.
    pub async fn unread(&mut self, limit: Option<u32>) -> Result<String, AppleError> {
        let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
        let v = run_jxa_json(&mut self.transport, &unread_expr(limit)).await?;
        Ok(crate::util::sanitize_json_text(
            &crate::util::unwrap_string_payload(v)?.to_string(),
        ))
    }

    /// Lists attachment metadata (name/mime/bytes/date), optionally scoped
    /// to one chat. Never returns content — fetch files via disk tools.
    pub async fn attachments(
        &mut self,
        chat: Option<String>,
        limit: Option<u32>,
        offset: u32,
    ) -> Result<String, AppleError> {
        let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
        let v = run_jxa_json(
            &mut self.transport,
            &attachments_expr(chat.as_deref(), limit, offset),
        )
        .await?;
        Ok(crate::util::sanitize_json_text(
            &crate::util::unwrap_string_payload(v)?.to_string(),
        ))
    }

    /// Sends an iMessage/SMS. Soft-gated like [`crate::mail::MailToolset::send`].
    pub async fn send(
        &mut self,
        to: &str,
        body: &str,
        token: Option<&str>,
    ) -> Result<String, AppleError> {
        let payload = json!({ "to": to, "body": body });
        let outcome = match self.gate.as_mut() {
            Some(gate) => gate
                .check("messages.send", &payload, token)
                .await
                .map_err(|e| AppleError::Transport(format!("gate error: {e}")))?,
            None => {
                return Err(AppleError::Transport(String::from(
                    "soft gate not configured — refusing to send",
                )));
            }
        };
        match outcome {
            GateOutcome::Confirm { payload, token } => Ok(json!({
                "status": "requires_confirmation",
                "payload": payload,
                "confirmation_token": token,
                "note": "re-invoke with confirmation_token",
            })
            .to_string()),
            GateOutcome::Execute => {
                run_jxa_json(&mut self.transport, &send_expr(to, body)).await?;
                Ok(json!({ "status": "sent", "to": to }).to_string())
            }
        }
    }
}

// --- JXA expression builders -------------------------------------------------

fn sql_str(s: &str) -> String {
    // SQL string literal with single quotes doubled.
    format!("'{}'", s.replace('\'', "''"))
}

/// Builds ONE `sqlite3` command emitting a `TOTAL` header line followed by
/// the paged rows, so count and page are consistent by construction (the
/// previous two-command version reported the global DB size and could
/// disagree with the page). Newlines are stripped from text in SQL because
/// sqlite3 emits one line per row.
fn read_expr(chat: Option<&str>, limit: u32, offset: u32) -> String {
    let db = "$HOME/Library/Messages/chat.db";
    let chat_filter = match chat {
        Some(c) => format!(
            " AND (c.chat_identifier={} OR h.id={} OR c.display_name={})",
            sql_str(c),
            sql_str(c),
            sql_str(c)
        ),
        None => String::new(),
    };
    let from_join = "FROM message m \
             LEFT JOIN handle h ON h.ROWID=m.handle_id \
             LEFT JOIN chat_message_join cmj ON cmj.message_id=m.ROWID \
             LEFT JOIN chat c ON c.ROWID=cmj.chat_id";
    let cmd = format!(
        "sqlite3 -separator '|||' {db} \"\
         SELECT COUNT(*) {from_join} \
           WHERE 1=1 AND COALESCE(m.associated_message_type,0)=0{chat_filter}; \
         SELECT json_object(\
           'from', COALESCE(CASE WHEN m.is_from_me=1 THEN 'me' ELSE h.id END,'unknown'),\
           'direction', CASE WHEN m.is_from_me=1 THEN 'out' ELSE 'in' END,\
           'text', REPLACE(REPLACE(COALESCE(m.text,''),char(13),' '),char(10),' '),\
           'date', REPLACE(datetime(m.date/1000000000+978307200,'unixepoch'),' ','T')||'Z') \
         {from_join} WHERE 1=1 AND COALESCE(m.associated_message_type,0)=0{chat_filter} \
         ORDER BY m.date DESC LIMIT {limit} OFFSET {offset};\""
    );
    sql_shell_lines_js(&cmd)
        + &(r#"  const total = Number(lines[0]) || 0;
  const messages = [];
  for (const line of lines.slice(1)) {
    if (!line) continue;
    const o = JSON.parse(line);
    messages.push({from: o.from, direction: o.direction,
                   text: o.text == null ? '' : String(o.text), date: o.date});
  }
  const payload = {total: total, messages: messages};
  if (messages.length < "#
            .to_string()
            + &limit.to_string()
            + ") payload.more = true;\n  return JSON.stringify(payload);\n})()\n")
}

fn chats_expr(limit: u32, offset: u32) -> String {
    let db = "$HOME/Library/Messages/chat.db";
    let cmd = format!(
        "sqlite3 -separator '|||' {db} \"\
         SELECT COUNT(*) FROM chat; \
         SELECT json_object(\
           'identifier', COALESCE(NULLIF(c.chat_identifier,''),'unknown'),\
           'display_name', COALESCE(c.display_name,''),\
           'service', COALESCE(c.service_name,''),\
           'handle', COALESCE((SELECT h.id FROM handle h JOIN chat_handle_join chj \
             ON chj.handle_id=h.ROWID WHERE chj.chat_id=c.ROWID LIMIT 1),''),\
           'message_count', (SELECT COUNT(*) FROM chat_message_join cmj2 \
             WHERE cmj2.chat_id=c.ROWID),\
           'last_activity', datetime(COALESCE(MAX(cmj3.message_date),0)/1000000000+978307200,'unixepoch'),\
           'participant_count', (SELECT COUNT(DISTINCT chj2.handle_id) FROM chat_handle_join chj2 \
             WHERE chj2.chat_id=c.ROWID),\
           'participants', COALESCE((SELECT GROUP_CONCAT(DISTINCT h3.id) FROM chat_handle_join chj3 \
             JOIN handle h3 ON h3.ROWID=chj3.handle_id WHERE chj3.chat_id=c.ROWID),'')) \
         FROM chat c \
         LEFT JOIN chat_message_join cmj3 ON cmj3.chat_id=c.ROWID \
         GROUP BY c.ROWID ORDER BY MAX(cmj3.message_date) DESC \
         LIMIT {limit} OFFSET {offset};\""
    );
    sql_shell_lines_js(&cmd)
        + r#"  const total = Number(lines[0]) || 0;
  const chats = [];
  for (const line of lines.slice(1)) {
    if (!line) continue;
    const o = JSON.parse(line);
    const pc = Number(o.participant_count) || 0;
    chats.push({identifier: o.identifier, display_name: o.display_name,
                service: o.service, handle: o.handle,
                message_count: Number(o.message_count) || 0,
                last_activity: o.last_activity,
                is_group: pc > 1,
                participants: o.participants ? String(o.participants).split(',').slice(0, 8) : [],
                participant_count: pc});
  }
  return JSON.stringify({total: total, chats: chats});
})()"#
}

fn unread_expr(limit: u32) -> String {
    let db = "$HOME/Library/Messages/chat.db";
    let from_join = "FROM message m \
             JOIN chat_message_join cmj ON cmj.message_id=m.ROWID \
             JOIN chat c ON c.ROWID=cmj.chat_id";
    let where_clause = "WHERE m.is_from_me=0 AND m.is_read=0";
    let cmd = format!(
        "sqlite3 -separator '|||' {db} \"\
         SELECT COUNT(*) FROM (SELECT DISTINCT c.ROWID {from_join} {where_clause}); \
         SELECT json_object(\
           'chat', COALESCE(NULLIF(c.chat_identifier,''),'unknown'),\
           'display_name', COALESCE(c.display_name,''),\
           'unread', COUNT(*),\
           'last_activity', datetime(MAX(m.date)/1000000000+978307200,'unixepoch')) \
         {from_join} {where_clause} GROUP BY c.ROWID \
         ORDER BY COUNT(*) DESC LIMIT {limit};\""
    );
    sql_shell_lines_js(&cmd)
        + r#"  const total = Number(lines[0]) || 0;
  const chats = [];
  for (const line of lines.slice(1)) {
    if (!line) continue;
    const o = JSON.parse(line);
    chats.push({chat: o.chat, display_name: o.display_name,
                unread: Number(o.unread) || 0, last_activity: o.last_activity});
  }
  return JSON.stringify({total: total, chats: chats});
})()"#
}

fn attachments_expr(chat: Option<&str>, limit: u32, offset: u32) -> String {
    let db = "$HOME/Library/Messages/chat.db";
    let from_join = "FROM attachment a \
             JOIN message_attachment_join maj ON maj.attachment_id=a.ROWID \
             JOIN message m ON m.ROWID=maj.message_id \
             LEFT JOIN chat_message_join cmj ON cmj.message_id=m.ROWID \
             LEFT JOIN chat c ON c.ROWID=cmj.chat_id";
    let chat_filter = match chat {
        Some(c) => format!(
            " WHERE (c.chat_identifier={} OR c.display_name={})",
            sql_str(c),
            sql_str(c)
        ),
        None => String::new(),
    };
    let cmd = format!(
        "sqlite3 -separator '|||' {db} \"\
         SELECT COUNT(*) {from_join}{chat_filter}; \
         SELECT json_object(\
           'name', COALESCE(a.transfer_name,'unnamed'),\
           'mime', COALESCE(a.mime_type,''),\
           'bytes', COALESCE(a.total_bytes,0),\
           'date', COALESCE(datetime(m.date/1000000000+978307200,'unixepoch'),''),\
           'chat', COALESCE(c.chat_identifier,'')) \
         {from_join}{chat_filter} ORDER BY m.date DESC \
         LIMIT {limit} OFFSET {offset};\""
    );
    sql_shell_lines_js(&cmd)
        + &(r#"  const total = Number(lines[0]) || 0;
  const attachments = [];
  for (const line of lines.slice(1)) {
    if (!line) continue;
    const o = JSON.parse(line);
    attachments.push({name: o.name, mime: o.mime,
                      bytes: Number(o.bytes) || 0, date: o.date, chat: o.chat});
  }
  return JSON.stringify({total: total, offset: "#
            .to_string()
            + &offset.to_string()
            + ", limit: "
            + &limit.to_string()
            + ", attachments: attachments});\n})()\n")
}

fn send_expr(to: &str, body: &str) -> String {
    format!(
        r#"(() => {{
  const M = Application('Messages');
  for (const svc of M.services()) {{
    if (!svc.enabled()) continue;
    const hits = svc.participants.whose({{handle: {}}})();
    if (hits.length > 0) {{
      M.send({}, {{to: hits[0]}});
      return {{status: 'sent', service: svc.name()}};
    }}
  }}
  throw new Error('participant not found on any enabled service: {}');
}})()"#,
        js_str(to),
        js_str(body),
        js_str(to),
    )
}

/// Test seams: builder visibility without widening the public API beyond
/// what integration contracts need.
pub fn unread_expr_for_test(limit: u32) -> String {
    unread_expr(limit)
}
pub fn attachments_expr_for_test(chat: Option<&str>, limit: u32, offset: u32) -> String {
    attachments_expr(chat, limit, offset)
}
pub fn read_expr_for_test(chat: Option<&str>, limit: u32, offset: u32) -> String {
    read_expr(chat, limit, offset)
}
