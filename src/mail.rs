//! Apple Mail tools.
//!
//! [`MailToolset`] is the whole group: transport + soft gate, fully testable
//! against [`personai_core::macos::MockTransport`]. All scripts are JXA
//! (JavaScript) executed through `personai_core::macos::run_jxa_json`.
//!
//! Context discipline: `search` returns metadata only (never bodies),
//! paginated (`total`/`offset`/`limit`), capped at [`MAX_LIMIT`].

use personai_core::macos::{AppleError, AppleTransport, run_jxa_json};
use personai_core::safety::{GateOutcome, SoftGate};
use serde_json::{Value, json};

use crate::util::js_str;
use crate::{DEFAULT_LIMIT, MAX_LIMIT};

/// Apple Mail tool group over any transport.
pub struct MailToolset<T: AppleTransport> {
    pub transport: T,
    /// `Some` in production (token store under the state dir); tests may
    /// leave `None` to prove gated calls refuse.
    pub gate: Option<SoftGate>,
}

impl<T: AppleTransport> MailToolset<T> {
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

    /// Lists configured Mail accounts with identity fields, so agents can
    /// reason about which account is which (display names alone are often
    /// just "Google" / "Exchange").
    pub async fn list_accounts(&mut self) -> Result<String, AppleError> {
        let v = run_jxa_json(
            &mut self.transport,
            "(() => { const M = Application('Mail'); \
             return M.accounts().map(a => ({ \
               name: a.name(), \
               email: (a.emailAddresses()[0] || ''), \
               accountType: String(a.accountType()), \
               enabled: a.enabled() \
             })); })()",
        )
        .await?;
        Ok(json!({ "accounts": v }).to_string())
    }

    /// Lists mailboxes (per account, with message counts) so agents can
    /// discover where mail actually lives before searching — Gmail labels
    /// like Work/Important are separate mailboxes outside the inbox.
    pub async fn list_mailboxes(&mut self, account: Option<String>) -> Result<String, AppleError> {
        let v = run_jxa_json(&mut self.transport, &mailboxes_expr(account.as_deref())).await?;
        Ok(v.to_string())
    }

    /// Searches Mail metadata. Returns `{total, offset, limit, results}`
    /// where each result carries `id, subject, from, date, snippet` and
    /// NEVER a body.
    pub async fn search(
        &mut self,
        query: &str,
        account: Option<&str>,
        mailbox: Option<String>,
        since: Option<&str>,
        limit: Option<u32>,
        offset: u32,
    ) -> Result<String, AppleError> {
        let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
        let v = run_jxa_json(
            &mut self.transport,
            &search_expr(query, account, mailbox.as_deref(), since, limit, offset),
        )
        .await?;

        let total = v.get("total").and_then(Value::as_u64).unwrap_or(0) as u32;
        let mut results = v
            .get("results")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        // Defense in depth: even if a script ever returned bodies, the
        // toolset strips them and caps the page.
        for r in results.iter_mut() {
            if let Some(o) = r.as_object_mut() {
                o.remove("body");
                o.remove("content");
            }
        }
        results.truncate(limit as usize);
        Ok(json!({
            "total": total,
            "offset": offset,
            "limit": limit,
            "results": results,
        })
        .to_string())
    }

    /// Reads one full message by id (from `search` results).
    pub async fn read(&mut self, id: &str) -> Result<String, AppleError> {
        let v = run_jxa_json(&mut self.transport, &read_expr(id)).await?;
        Ok(v.to_string())
    }

    /// Sends an email. Without a valid `token` the send does not happen:
    /// the caller gets back the exact payload plus a fresh confirmation
    /// token instead.
    pub async fn send(
        &mut self,
        to: &str,
        subject: &str,
        body: &str,
        token: Option<&str>,
    ) -> Result<String, AppleError> {
        let payload = json!({ "to": to, "subject": subject, "body": body });
        let outcome = match self.gate.as_mut() {
            Some(gate) => gate
                .check("mail.send", &payload, token)
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
                "note": "Show this payload to the user; re-invoke mail_send with confirmation_token to execute.",
            })
            .to_string()),
            GateOutcome::Execute => {
                run_jxa_json(&mut self.transport, &send_expr(to, subject, body)).await?;
                Ok(json!({ "status": "sent", "to": to, "subject": subject }).to_string())
            }
        }
    }
}

// --- JXA expression builders -------------------------------------------------
//
// Each builder returns a single JS *expression* (an arrow IIFE) embedded by
// `wrap_jxa`. User text enters only through `js_str`.

/// Scans the newest [`SCAN_MAX`] messages of the target mailbox with four
/// bulk Apple Events, filters in JS, pages the matches. Individual access
/// through a `whose(...)` specifier re-evaluates the whole query per item
/// and times out on real mailboxes.
const SCAN_MAX: u32 = 1000;

fn search_expr(
    query: &str,
    account: Option<&str>,
    mailbox: Option<&str>,
    since: Option<&str>,
    limit: u32,
    offset: u32,
) -> String {
    let q = js_str(&query.to_lowercase());
    // Default: the unified inbox (all accounts). `account` narrows to that
    // account's inbox; `mailbox` targets a named mailbox inside `account`
    // (Gmail labels live there, outside the inbox).
    let account_clause = match (account, mailbox) {
        (Some(a), Some(mb)) => format!(
            "let box = M.accounts.whose({{name: {a}}})()[0].mailboxes.whose({{name: {mb}}})()[0];",
            a = js_str(a),
            mb = js_str(mb)
        ),
        (Some(a), None) => format!(
            "let box = M.accounts.whose({{name: {}}})()[0].inbox();",
            js_str(a)
        ),
        (None, Some(mb)) => format!(
            "let box = M.mailboxes.whose({{name: {}}})()[0];",
            js_str(mb)
        ),
        (None, None) => "let box = M.inbox();".to_string(),
    };
    // All narrowing happens in JS over one bulk metadata fetch — Mail-side
    // `whose` results cannot take bulk property gets, and per-item
    // specifier access re-evaluates the query (times out on large boxes).
    let since_clause = match since {
        Some(iso) => format!("const sinceMs = Date.parse({});", js_str(iso)),
        None => "const sinceMs = null;".to_string(),
    };
    format!(
        r#"(() => {{
  const M = Application('Mail');
  {account_clause}
  {since_clause}
  const scan = Math.min(box.messages.length, {SCAN_MAX});
  const ids = box.messages.id().slice(0, scan);
  const subjects = box.messages.subject().slice(0, scan);
  const senders = box.messages.sender().slice(0, scan);
  const dates = box.messages.dateReceived().slice(0, scan);
  const qLower = {q};
  const idx = [];
  for (let i = 0; i < ids.length; i++) {{
    if (sinceMs !== null && (!dates[i] || dates[i].getTime() < sinceMs)) continue;
    if ((subjects[i] && subjects[i].toLowerCase().includes(qLower)) ||
        (senders[i] && senders[i].toLowerCase().includes(qLower))) idx.push(i);
  }}
  const total = idx.length;
  const end = Math.min({offset} + {limit}, total);
  const out = [];
  for (let k = {offset}; k < end; k++) {{
    const i = idx[k];
    const m = box.messages[i];
    let snippet = '';
    try {{ snippet = String(m.content()).slice(0, 140); }} catch (e) {{}}
    out.push({{
      id: String(ids[i]),
      subject: subjects[i],
      from: senders[i],
      date: dates[i].toISOString(),
      snippet: snippet,
    }});
  }}
  return {{total: total, results: out}};
}})()"#
    )
}

/// Enumerates mailboxes with message counts. Full sweep of 3 accounts /
/// ~42 mailboxes measured at ~2 s live; `account` narrows to one account.
fn mailboxes_expr(account: Option<&str>) -> String {
    let account_clause = match account {
        Some(name) => format!(
            "const accounts = M.accounts.whose({{name: {}}})().map(a => [a.name(), a]);",
            js_str(name)
        ),
        None => "const accounts = M.accounts().map(a => [a.name(), a]);".to_string(),
    };
    format!(
        r#"(() => {{
  const M = Application('Mail');
  {account_clause}
  const mailboxes = [];
  for (const [acctName, a] of accounts) {{
    for (const mb of a.mailboxes()) {{
      let count = 0;
      try {{ count = mb.messages.length; }} catch (e) {{}}
      mailboxes.push({{account: acctName, name: mb.name(), count: count}});
    }}
  }}
  return {{mailboxes: mailboxes}};
}})()"#
    )
}

fn read_expr(id: &str) -> String {
    let id = js_str(id);
    format!(
        r#"(() => {{
  const M = Application('Mail');
  const wanted = Number({id});
  const hits = M.inbox().messages.whose({{id: wanted}});
  if (hits.length === 0) throw new Error('message not found: ' + {id});
  const m = hits[0];
  let body = '';
  try {{ body = String(m.content()); }} catch (e) {{}}
  return {{
    id: String(m.id()),
    subject: m.subject(),
    from: m.sender(),
    date: m.dateReceived().toISOString(),
    body: body,
  }};
}})()"#
    )
}

fn send_expr(to: &str, subject: &str, body: &str) -> String {
    format!(
        r#"(() => {{
  const M = Application('Mail');
  const msg = M.OutgoingMessage({{subject: {}, content: {}}}).make();
  msg.to.push(M.ToRecipient({{address: {}}}).make());
  msg.send();
  return {{status: 'sent'}};
}})()"#,
        js_str(subject),
        js_str(body),
        js_str(to),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_builder_escapes_user_text() {
        let s = search_expr("O'Brien \"x\"", Some("work"), None, None, 20, 0);
        assert!(s.contains(r#""o'brien \"x\"""#), "{s}");
        assert!(s.contains(r#"{name: "work"}"#), "{s}");
    }
}
