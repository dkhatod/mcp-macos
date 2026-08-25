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

use crate::util::js_str;
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
         SELECT COUNT(*) {from_join} WHERE 1=1{chat_filter}; \
         SELECT COALESCE(CASE WHEN m.is_from_me=1 THEN 'me' ELSE h.id END,'unknown'),\
         CASE WHEN m.is_from_me=1 THEN 'out' ELSE 'in' END,\
         REPLACE(REPLACE(REPLACE(COALESCE(m.text,''),char(13),' '),char(10),' '),'|||','/'),\
         REPLACE(datetime(m.date/1000000000+978307200,'unixepoch'),' ','T')||'Z' \
         {from_join} WHERE 1=1{chat_filter} ORDER BY m.date DESC \
         LIMIT {limit} OFFSET {offset};\""
    );
    format!(
        r#"(() => {{
  const app = Application.currentApplication();
  app.includeStandardAdditions = true;
  const out = String(app.doShellScript({}));
  // Stored text can carry characters SQL's CR/LF stripping misses
  // (vertical tab, U+2028...); they would corrupt the single-line payload.
  const clean = s2 => String(s2 == null ? '' : s2)
    .replace(/[\u0000-\u001F\u007F\u2028\u2029]/g, ' ');
  // doShellScript emits CR-delimited text — accept CR, CRLF and LF.
  const lines = out.length === 0 ? [] : out.split(/\r\n|\r|\n/);
  const total = Number(lines[0]) || 0;
  const messages = lines.slice(1).map(line => {{
    const [from, direction, text, date] = line.split('|||');
    return {{from: clean(from), direction: clean(direction),
            text: clean(text), date: clean(date)}};
  }});
  const payload = {{total: total, messages: messages}};
  if (messages.length < {}) payload.more = true;
  return JSON.stringify(payload);
}})()"#,
        js_str(&cmd),
        limit,
    )
}

/// Chat discovery: identifier, display name, service, a sample participant
/// handle, message count, and last activity — recency-ordered, paginated.
/// This is how agents resolve "send to NAME" without guessing handles.
fn chats_expr(limit: u32, offset: u32) -> String {
    let db = "$HOME/Library/Messages/chat.db";
    let cmd = format!(
        "sqlite3 -separator '|||' {db} \"\
         SELECT COUNT(*) FROM chat; \
         SELECT COALESCE(NULLIF(c.chat_identifier,''),'unknown'),\
         COALESCE(REPLACE(c.display_name,'|||','/'),''),COALESCE(c.service_name,''),\
         COALESCE((SELECT h.id FROM handle h JOIN chat_handle_join chj \
           ON chj.handle_id=h.ROWID WHERE chj.chat_id=c.ROWID LIMIT 1),''),\
         (SELECT COUNT(*) FROM chat_message_join cmj2 WHERE cmj2.chat_id=c.ROWID),\
         datetime(COALESCE(MAX(cmj3.message_date),0)/1000000000+978307200,'unixepoch'),\
         (SELECT COUNT(DISTINCT chj2.handle_id) FROM chat_handle_join chj2 \
           WHERE chj2.chat_id=c.ROWID),\
         COALESCE((SELECT GROUP_CONCAT(DISTINCT h3.id) FROM chat_handle_join chj3 \
           JOIN handle h3 ON h3.ROWID=chj3.handle_id WHERE chj3.chat_id=c.ROWID),'') \
         FROM chat c \
         LEFT JOIN chat_message_join cmj3 ON cmj3.chat_id=c.ROWID \
         GROUP BY c.ROWID ORDER BY MAX(cmj3.message_date) DESC \
         LIMIT {limit} OFFSET {offset};\""
    );
    format!(
        r#"(() => {{
  const app = Application.currentApplication();
  app.includeStandardAdditions = true;
  const out = String(app.doShellScript({}));
  // doShellScript emits CR-delimited text — accept CR, CRLF and LF.
  const lines = out.length === 0 ? [] : out.split(/\r\n|\r|\n/);
  const total = Number(lines[0]) || 0;
    // Stored display names carry the same control-char hazard as text.
    const clean = s2 => String(s2 == null ? '' : s2)
      .replace(/[\u0000-\u001F\u007F\u2028\u2029]/g, ' ');
    const chats = lines.slice(1).map(line => {{
      const [identifier, display_name, service, handle, message_count,
             last_activity, participant_count, participants] = line.split('|||');
      const pc = Number(participant_count) || 0;
      return {{identifier: clean(identifier), display_name: clean(display_name),
              service: clean(service), handle: clean(handle),
              message_count: Number(message_count) || 0,
              last_activity: clean(last_activity),
              is_group: pc > 1,
              participants: participants ? clean(participants).split(',').slice(0, 8) : [],
              participant_count: pc}};
    }});
    return JSON.stringify({{total: total, chats: chats}});
}})()"#,
        js_str(&cmd),
    )
}

// --- Unread digest -----------------------------------------------------------

/// Per-chat unread counts (`is_from_me=0 AND is_read=0`), heaviest chats
/// first — the input for any triage or digest flow.
fn unread_expr(limit: u32) -> String {
    let db = "$HOME/Library/Messages/chat.db";
    let from_join = "FROM message m \
             JOIN chat_message_join cmj ON cmj.message_id=m.ROWID \
             JOIN chat c ON c.ROWID=cmj.chat_id";
    let where_clause = "WHERE m.is_from_me=0 AND m.is_read=0";
    let cmd = format!(
        "sqlite3 -separator '|||' {db} \"\
         SELECT COUNT(*) FROM (SELECT DISTINCT c.ROWID {from_join} {where_clause}); \
         SELECT COALESCE(NULLIF(c.chat_identifier,''),'unknown'),\
         COALESCE(REPLACE(c.display_name,'|||','/'),''),COUNT(*),\
         datetime(MAX(m.date)/1000000000+978307200,'unixepoch') \
         {from_join} {where_clause} GROUP BY c.ROWID \
         ORDER BY COUNT(*) DESC LIMIT {limit};\""
    );
    format!(
        r#"(() => {{
  const app = Application.currentApplication();
  app.includeStandardAdditions = true;
  const out = String(app.doShellScript({}));
  const clean = s2 => String(s2 == null ? '' : s2)
    .replace(/[ -  ]/g, ' ');
  const lines = out.length === 0 ? [] : out.split(/
|
|
/);
  const total = Number(lines[0]) || 0;
  const chats = lines.slice(1).map(line => {{
    const [chat, display_name, unread, last_activity] = line.split('|||');
    return {{chat: clean(chat), display_name: clean(display_name),
            unread: Number(unread) || 0, last_activity: clean(last_activity)}};
  }});
  return JSON.stringify({{total: total, chats: chats}});
}})()"#,
        crate::util::js_str(&cmd)
    )
}

// --- Attachment listing -------------------------------------------------------

/// Attachment METADATA only (name/mime/size/date) — never blob content.
fn attachments_expr(chat: Option<&str>, limit: u32, offset: u32) -> String {
    let db = "$HOME/Library/Messages/chat.db";
    let from_join = "FROM attachment a \
             JOIN message_attachment_join maj ON maj.attachment_id=a.ROWID \
             JOIN message m ON m.ROWID=maj.message_id \
             LEFT JOIN chat_message_join cmj ON cmj.message_id=m.ROWID \
             LEFT JOIN chat c ON c.ROWID=cmj.chat_id";
    let chat_filter = match chat {
        Some(c) => format!(
            " WHERE (c.chat_identifier={} OR h.id={} OR c.display_name={})",
            sql_str(c),
            // attachments join messages whose sender handle is the OTHER
            // party's handle row; h is not joined above, so match by chat
            // identity columns only.
            sql_str(c),
            sql_str(c)
        ),
        None => String::new(),
    };
    let _ = &from_join;
    let cmd = format!(
        "sqlite3 -separator '|||' {db} \"\
         SELECT COUNT(*) {from_join}{chat_filter}; \
         SELECT COALESCE(REPLACE(a.transfer_name,'|||','/'),'unnamed'),\
         COALESCE(a.mime_type,''),COALESCE(a.total_bytes,0),\
         COALESCE(datetime(m.date/1000000000+978307200,'unixepoch'),'') \
         {from_join}{chat_filter} ORDER BY m.date DESC \
         LIMIT {limit} OFFSET {offset};\""
    );
    format!(
        r#"(() => {{
  const app = Application.currentApplication();
  app.includeStandardAdditions = true;
  const out = String(app.doShellScript({}));
  const clean = s2 => String(s2 == null ? '' : s2)
    .replace(/[ -  ]/g, ' ');
  const lines = out.length === 0 ? [] : out.split(/
|
|
/);
  const total = Number(lines[0]) || 0;
  const attachments = lines.slice(1).map(line => {{
    const [name, mime, bytes, date] = line.split('|||');
    return {{name: clean(name), mime: clean(mime),
            bytes: Number(bytes) || 0, date: clean(date)}};
  }});
  return JSON.stringify({{total: total, offset: {offset}, limit: {limit}, attachments: attachments}});
}})()"#,
        crate::util::js_str(&cmd)
    )
}

fn send_expr(to: &str, body: &str) -> String {
    format!(
        r#"(() => {{
  const M = Application('Messages');
  const svc = M.services().find(s => s.enabled());
  if (!svc) throw new Error('no enabled Messages service');
  const hits = svc.participants.whose({{handle: {}}})();
  if (hits.length === 0) throw new Error('participant not found: {}');
  M.send({}, {{to: hits[0]}});
  return {{status: 'sent'}};
}})()"#,
        js_str(to),
        js_str(to),
        js_str(body),
    )
}

#[cfg(test)]
mod builder_tests {
    use super::*;

    #[test]
    fn chats_builder_counts_participants_for_group_detection() {
        let script = chats_expr(20, 0);
        assert!(
            script.contains("COUNT(DISTINCT chj2.handle_id)"),
            "{script}"
        );
        assert!(script.contains("GROUP_CONCAT(DISTINCT h3.id)"), "{script}");
    }

    #[test]
    fn read_builder_strips_separator_collisions_from_text() {
        let script = read_expr(Some("a@x"), 10, 0);
        assert!(script.contains("'|||','/'"), "{script}");
    }

    #[test]
    fn mappers_split_on_every_line_ending_style() {
        // doShellScript returns CR-delimited output (classic JXA quirk):
        // splitting on '\n' alone collapsed the whole blob into one line,
        // yielding silent total:0 against EVERY live mailbox.
        for script in [read_expr(Some("a@x"), 10, 0), chats_expr(20, 0)] {
            assert!(
                script.contains("split(/\\r\\n|\\r|\\n/)"),
                "mapper must handle CR/CRLF/LF: {script}"
            );
        }
    }
}

#[cfg(test)]
mod debug_probe {
    use super::*;

    #[test]
    #[ignore]
    fn print_read_expr() {
        let expr = read_expr(Some("+17038140603"), 2, 0);
        std::fs::write("/tmp/full_expr.txt", &expr).unwrap();
        eprintln!("WROTE /tmp/full_expr.txt ({} bytes)", expr.len());
    }
}

/// Test seams: builder visibility without widening the public API beyond
/// what integration contracts need.
pub fn unread_expr_for_test(limit: u32) -> String {
    unread_expr(limit)
}
pub fn attachments_expr_for_test(chat: Option<&str>, limit: u32, offset: u32) -> String {
    attachments_expr(chat, limit, offset)
}
