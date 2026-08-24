//! iMessage/SMS tools.
//!
//! `read` goes through `sqlite3` on the chat.db (the only reliable history
//! source; needs Full Disk Access), executed inside the JXA shell so it
//! stays behind the same transport + error taxonomy as every other group.
//! `send` is soft-gated.
//!
//! Context discipline: `read` is paginated (`total`/`offset`/`limit`) and
//! returns per-message metadata only.

use personai_core::macos::{AppleError, AppleTransport, run_jxa_json};
use personai_core::safety::{GateOutcome, SoftGate};
use serde_json::json;

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
        let v = run_jxa_json(
            &mut self.transport,
            &read_expr(chat.as_deref(), limit, offset),
        )
        .await?;
        Ok(crate::util::unwrap_string_payload(v)?.to_string())
    }

    /// Lists chats (identifier, display name, service, sample handle,
    /// message count, last activity), most recent first. Auto-tier.
    pub async fn chats(&mut self, limit: Option<u32>, offset: u32) -> Result<String, AppleError> {
        let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
        let v = run_jxa_json(&mut self.transport, &chats_expr(limit, offset)).await?;
        Ok(crate::util::unwrap_string_payload(v)?.to_string())
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
         REPLACE(REPLACE(COALESCE(m.text,''),char(13),' '),char(10),' '),\
         REPLACE(datetime(m.date/1000000000+978307200,'unixepoch'),' ','T')||'Z' \
         {from_join} WHERE 1=1{chat_filter} ORDER BY m.date DESC \
         LIMIT {limit} OFFSET {offset};\""
    );
    format!(
        r#"(() => {{
  const app = Application.currentApplication();
  app.includeStandardAdditions = true;
  const out = String(app.doShellScript({}));
  const lines = out.length === 0 ? [] : out.split('\n');
  const total = Number(lines[0]) || 0;
  const messages = lines.slice(1).map(line => {{
    const [from, direction, text, date] = line.split('|||');
    return {{from: from, direction: direction, text: text || '', date: date}};
  }});
  const rowJson = messages.map(m => JSON.stringify(m)).join(',\n');
  let payload = '{{"total":' + total + ',"messages":[\n' + rowJson + '\n]}}';
  if (messages.length < {}) payload += ',"more":true';
  return payload;
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
         COALESCE(c.display_name,''),COALESCE(c.service_name,''),\
         COALESCE((SELECT h.id FROM handle h JOIN chat_handle_join chj \
           ON chj.handle_id=h.ROWID WHERE chj.chat_id=c.ROWID LIMIT 1),''),\
         (SELECT COUNT(*) FROM chat_message_join cmj2 WHERE cmj2.chat_id=c.ROWID),\
         datetime(COALESCE(MAX(cmj3.message_date),0)/1000000000+978307200,'unixepoch') \
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
  const lines = out.length === 0 ? [] : out.split('\n');
  const total = Number(lines[0]) || 0;
  const chats = lines.slice(1).map(line => {{
    const [identifier, display_name, service, handle, message_count, last_activity] = line.split('|||');
    return {{identifier: identifier, display_name: display_name, service: service,
            handle: handle, message_count: Number(message_count) || 0,
            last_activity: last_activity}};
  }});
  const rowJson = chats.map(c => JSON.stringify(c)).join(',\n');
  return '{{"total":' + total + ',"chats":[\n' + rowJson + '\n]}}';
}})()"#,
        js_str(&cmd),
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
