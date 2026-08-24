//! Apple Mail tools.
//!
//! [`MailToolset`] is the whole group: transport + soft gate, fully testable
//! against [`personai_core::macos::MockTransport`]. All scripts are JXA
//! (JavaScript) executed through `personai_core::macos::run_jxa_json`.
//!
//! Context discipline: `search` returns metadata only (never bodies),
//! paginated (`total`/`offset`/`limit`), capped at [`MAX_LIMIT`].

use personai_core::macos::{AppleError, AppleTransport, run_jxa_json, wrap_jxa};
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

/// Search targets for [`MailToolset::search_multi`].
#[derive(Clone, Debug)]
pub enum MailTargets {
    /// The unified inbox across every account.
    Unified,
    /// Explicit `(account, mailbox)` pairs.
    Folders(Vec<(String, String)>),
}
/// Aggregation mode for [`MailToolset::search_multi`]: collapse matches
/// into per-sender or per-normalized-subject groups instead of returning
/// rows, so agents triage hundreds of hits in one page.
#[derive(Clone, Debug)]
pub enum MailGroupBy {
    /// Group by sender email address (angle-bracket form) or raw sender.
    Sender,
    /// Group by normalized subject (casefold, strip Re:/Fwd: prefixes).
    Subject,
}

impl MailGroupBy {
    /// Parses the wire string accepted by `mail_search`'s `group_by`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "sender" => Some(Self::Sender),
            "subject" => Some(Self::Subject),
            _ => None,
        }
    }
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
    /// Runs the soft-gate check shared by send/forward/reply. Refuses when
    /// no gate is configured.
    async fn gated(
        &mut self,
        action: &str,
        payload: &Value,
        token: Option<&str>,
    ) -> Result<GateOutcome, AppleError> {
        match self.gate.as_mut() {
            Some(gate) => gate
                .check(action, payload, token)
                .await
                .map_err(|e| AppleError::Transport(format!("gate error: {e}"))),
            None => Err(AppleError::Transport(format!(
                "soft gate not configured — refusing {action}"
            ))),
        }
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
        let matched = account.is_none()
            || v.get("mailboxes")
                .and_then(Value::as_array)
                .is_some_and(|a| !a.is_empty());
        if matched {
            return Ok(v.to_string());
        }
        // A silent [] for an unknown account sends agents guessing names;
        // echo the valid ones so the next call self-corrects.
        let accounts = run_jxa_json(
            &mut self.transport,
            "(() => { const M = Application('Mail'); return M.accounts().map(a => a.name()); })()",
        )
        .await?;
        let asked = account.as_deref().unwrap_or("");
        Ok(json!({
            "mailboxes": [],
            "note": format!(
                "no mailboxes matched account {:?} — use one of available_accounts",
                asked
            ),
            "available_accounts": accounts,
        })
        .to_string())
    }

    /// Lists mailboxes with counts plus each box's most recent activity
    /// (`last_activity` ISO timestamp or null, best-effort) so agents can
    /// pick live, active folders without opening Mail.
    pub async fn list_mailboxes_detailed(
        &mut self,
        account: Option<String>,
    ) -> Result<String, AppleError> {
        let v = run_jxa_json(
            &mut self.transport,
            &mailboxes_detailed_expr(account.as_deref()),
        )
        .await?;
        Ok(v.to_string())
    }

    /// Searches Mail metadata. `query` + `any_of` form the OR term set
    /// (both empty = match-all census). Returns
    /// `{total, offset, limit, results}` where each result carries
    /// `id, subject, from, date, snippet` and NEVER a body.
    // Argument lists mirror the mail_search wire params one-to-one; a
    // params struct would only rename positions.
    #[allow(clippy::too_many_arguments)]
    pub async fn search(
        &mut self,
        query: &str,
        any_of: &[String],
        account: Option<&str>,
        mailbox: Option<String>,
        since: Option<&str>,
        until: Option<&str>,
        limit: Option<u32>,
        offset: u32,
        scan: u32,
        snippets: bool,
    ) -> Result<String, AppleError> {
        let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
        let v = self
            .run_search_json(&search_expr(
                query,
                any_of,
                account,
                mailbox.as_deref(),
                since,
                until,
                limit,
                offset,
                scan,
                snippets,
            ))
            .await?;
        let total = v.get("total").and_then(Value::as_u64).unwrap_or(0) as u32;
        let timed_out = v.get("truncated").and_then(Value::as_bool).unwrap_or(false);
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
        // Token diet (no offset/limit echo; truncated only when true) and
        // one record per line so client compactors drop whole records
        // instead of slicing a single giant JSON line mid-array.
        let mut payload = format!(
            "{{\"total\":{},\"results\":[\n{}\n]}}",
            total,
            crate::util::join_rows(&results)
        );
        if timed_out {
            payload.pop();
            payload.push_str(",\"truncated\":true}}");
        }
        Ok(payload)
    }

    /// Searches one or more targets with a SINGLE script and merges the
    /// matches date-descending. A wall-clock budget ([`GLOBAL_BUDGET_MS`])
    /// is checked between targets: expiry stops the sweep early and
    /// `truncated: true` reports the partial coverage.
    ///
    /// Row mode (default) payload: `{total, results,
    /// scanned_per_folder[, truncated]}` where each result carries its
    /// `folder` tag (`"Account/Mailbox"`). With `group`, rows collapse into
    /// `{total, total_groups, groups, scanned_per_folder[, truncated]}`
    /// where each group carries `key, name, count, first, last,
    /// latest_id, latest_ids (3 newest — one sender often spans several
    /// threads/postings), sample_subjects (4), folders`. Metadata only — never bodies;
    /// `snippets = false` skips per-row body previews entirely (they cost
    /// one Apple Event each).
    #[allow(clippy::too_many_arguments)]
    pub async fn search_multi(
        &mut self,
        targets: &MailTargets,
        query: &str,
        any_of: &[String],
        since: Option<&str>,
        until: Option<&str>,
        limit: u32,
        offset: u32,
        group: Option<MailGroupBy>,
        scan: u32,
        snippets: bool,
    ) -> Result<String, AppleError> {
        let v = self
            .run_search_json(&search_multi_expr(
                targets, query, any_of, since, until, limit, offset, group, scan, snippets,
            ))
            .await?;

        let total = v.get("total").and_then(Value::as_u64).unwrap_or(0) as u32;
        let scanned = v
            .get("scanned_per_folder")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let truncated = v.get("truncated").and_then(Value::as_bool).unwrap_or(false);
        let truncated_field = if truncated { ",\"truncated\":true" } else { "" };
        if let Some(groups) = v.get("groups").cloned() {
            let groups = groups.as_array().cloned().unwrap_or_default();
            return Ok(format!(
                "{{\"total\":{},\"total_groups\":{},\"groups\":[\n{}\n],\"scanned_per_folder\":{}{}}}",
                total,
                v.get("total_groups").and_then(Value::as_u64).unwrap_or(0),
                crate::util::join_rows(&groups),
                scanned,
                truncated_field
            ));
        }
        let mut results = v
            .get("results")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        // Defense in depth, identical to `search`: even if a script ever
        // returned bodies, the toolset strips them and caps the page.
        for r in results.iter_mut() {
            if let Some(o) = r.as_object_mut() {
                o.remove("body");
                o.remove("content");
            }
        }
        results.truncate(limit as usize);
        Ok(format!(
            "{{\"total\":{},\"results\":[\n{}\n],\"scanned_per_folder\":{}{}}}",
            total,
            crate::util::join_rows(&results),
            scanned,
            truncated_field
        ))
    }

    /// Reads one full message by id (from `search` results). `folder`
    /// (`"Account/Mailbox"`, as tagged on the search row) targets the
    /// mailbox directly; without it the inbox is tried first, then a
    /// bounded per-mailbox sweep. Bodies are capped at [`READ_MAX_CHARS`]
    /// with `body_truncated: true`.
    pub async fn read(&mut self, id: &str, folder: Option<&str>) -> Result<String, AppleError> {
        // Unified-mode search tags rows "Unified/Inbox"; that is not a real
        // account/mailbox pair, so route those to the inbox-first sweep
        // instead of failing with `account not found: "Unified"`.
        let folder = folder.filter(|f| *f != "Unified/Inbox");
        let v = run_jxa_json(&mut self.transport, &read_expr(id, folder)).await?;
        Ok(v.to_string())
    }

    /// Forwards a message by id (from `search` results) to `to`. Without a
    /// valid `token` nothing is sent: the caller gets back the exact
    /// payload plus a fresh confirmation token instead. Mail's native
    /// forward pins the source message's account, preserving threading.
    pub async fn forward(
        &mut self,
        id: &str,
        to: &str,
        comment: Option<&str>,
        token: Option<&str>,
    ) -> Result<String, AppleError> {
        let payload = json!({ "id": id, "to": to, "comment": comment });
        match self.gated("mail.forward", &payload, token).await? {
            GateOutcome::Confirm { payload, token } => Ok(json!({
                "status": "requires_confirmation",
                "payload": payload,
                "confirmation_token": token,
                "note": "re-invoke with confirmation_token",
            })
            .to_string()),
            GateOutcome::Execute => {
                run_jxa_json(&mut self.transport, &forward_expr(id, to, comment)).await?;
                Ok(json!({ "status": "forwarded", "id": id, "to": to }).to_string())
            }
        }
    }

    /// Replies to a message by id (from `search` results) with `body`.
    /// Same soft-gate contract as [`MailToolset::forward`]: no token, no
    /// send — just the payload plus a fresh confirmation token.
    pub async fn reply(
        &mut self,
        id: &str,
        body: &str,
        token: Option<&str>,
    ) -> Result<String, AppleError> {
        let payload = json!({ "id": id, "body": body });
        match self.gated("mail.reply", &payload, token).await? {
            GateOutcome::Confirm { payload, token } => Ok(json!({
                "status": "requires_confirmation",
                "payload": payload,
                "confirmation_token": token,
                "note": "re-invoke with confirmation_token",
            })
            .to_string()),
            GateOutcome::Execute => {
                run_jxa_json(&mut self.transport, &reply_expr(id, body)).await?;
                Ok(json!({ "status": "replied", "id": id }).to_string())
            }
        }
    }

    /// Sends an email. Without a valid `token` the send does not happen:
    /// the caller gets back the exact payload plus a fresh confirmation
    /// token instead. `from` optionally selects the outgoing account by
    /// display name OR primary email address (case-insensitive); omitted,
    /// Mail uses the default account.
    pub async fn send(
        &mut self,
        to: &str,
        subject: &str,
        body: &str,
        from: Option<&str>,
        token: Option<&str>,
    ) -> Result<String, AppleError> {
        let mut payload = json!({ "to": to, "subject": subject, "body": body });
        if let Some(f) = from {
            payload["from"] = json!(f);
        }
        match self.gated("mail.send", &payload, token).await? {
            GateOutcome::Confirm { payload, token } => Ok(json!({
                "status": "requires_confirmation",
                "payload": payload,
                "confirmation_token": token,
                "note": "re-invoke with confirmation_token",
            })
            .to_string()),
            GateOutcome::Execute => {
                run_jxa_json(&mut self.transport, &send_expr(to, subject, body, from)).await?;
                Ok(json!({ "status": "sent", "to": to, "subject": subject }).to_string())
            }
        }
    }
}

// --- JXA expression builders -------------------------------------------------
//
// Each builder returns a single JS *expression* (an arrow IIFE) embedded by
// `wrap_jxa`. User text enters only through `js_str`.

/// Shared date-window preamble for search scripts: `since` floor,
/// `until` exclusive ceiling (both optional ISO 8601; native JS Date).
fn date_window_clause(since: Option<&str>, until: Option<&str>) -> String {
    format!(
        "const sinceMs = {};\n  const untilMs = {};",
        match since {
            Some(iso) => format!("Date.parse({})", js_str(iso)),
            None => "null".to_string(),
        },
        match until {
            Some(iso) => format!("Date.parse({})", js_str(iso)),
            None => "null".to_string(),
        }
    )
}

/// Emits the lowercased OR-term array shared by both search builders.
/// Empty array = match-all (census) mode; every message matches.
fn terms_js(query: &str, any_of: &[String]) -> String {
    let mut terms: Vec<String> = Vec::with_capacity(any_of.len() + 1);
    if !query.is_empty() {
        terms.push(query.to_lowercase());
    }
    terms.extend(
        any_of
            .iter()
            .filter(|t| !t.is_empty())
            .map(|t| t.to_lowercase()),
    );
    format!(
        "[{}]",
        terms
            .iter()
            .map(|t| js_str(t))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Wall-clock budget for searches, checked between [`MailTargets`] AND
/// inside the page-rendering loop; expiry yields a partial result with
/// `truncated: true` instead of exceeding the transport's 30 s osascript
/// kill window.
const GLOBAL_BUDGET_MS: u64 = 25_000;
/// Subprocess leash for search scripts. They self-terminate at
/// [`GLOBAL_BUDGET_MS`]; this only covers the case where bulk Apple Events
/// on a huge mailbox (Gmail-scale unified inbox) burn through the budget
/// before any check point — better a late partial page than a killed child.
const SEARCH_LEASE: std::time::Duration = std::time::Duration::from_secs(60);
/// Hard cap on `mail_read` body text. Full MIME dumps (newsletters,
/// base64 attachments) are context bombs for agent clients; callers who
/// need more than this should quote the source email instead.
const READ_MAX_CHARS: usize = 20_000;
impl<T: AppleTransport> MailToolset<T> {
    /// [`personai_core::macos::run_jxa_json`] with a longer leash: same
    /// `{"ok":true,"value"|"error":{number,desc}}` envelope contract, but
    /// search scripts get [`SEARCH_LEASE`] instead of the fixed 30 s kill.
    async fn run_search_json(&mut self, expr: &str) -> Result<Value, AppleError> {
        let raw = self.transport.run(&wrap_jxa(expr), SEARCH_LEASE).await?;
        let v: Value =
            serde_json::from_str(raw.trim()).map_err(|e| AppleError::Parse(e.to_string()))?;
        match v.get("ok") {
            Some(Value::Bool(true)) => Ok(v.get("value").cloned().unwrap_or(Value::Null)),
            _ => {
                let e = v.get("error").cloned().unwrap_or(Value::Null);
                Err(AppleError::AppleEvent {
                    number: e.get("number").and_then(|n| n.as_i64()).unwrap_or(-1),
                    desc: e
                        .get("desc")
                        .and_then(|d| d.as_str())
                        .unwrap_or("")
                        .to_string(),
                })
            }
        }
    }
}
#[allow(clippy::too_many_arguments)]
fn search_expr(
    query: &str,
    any_of: &[String],
    account: Option<&str>,
    mailbox: Option<&str>,
    since: Option<&str>,
    until: Option<&str>,
    limit: u32,
    offset: u32,
    scan: u32,
    snippets: bool,
) -> String {
    let terms_js = terms_js(query, any_of);
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
    let window_clause = date_window_clause(since, until);
    format!(
        r#"(() => {{
  const M = Application('Mail');
  {account_clause}
  {window_clause}
  const BUDGET_MS = {GLOBAL_BUDGET_MS};
  const t0 = Date.now();
  let timedOut = false;
  function in_window(d, lo, hi) {{
    if (!d) return lo === null && hi === null;
    const t = d.getTime();
    if (lo !== null && t < lo) return false;
    if (hi !== null && t >= hi) return false;
    return true;
  }}
  const scan = Math.min(box.messages.length, {scan});
  const ids = box.messages.id().slice(0, scan);
  const subjects = box.messages.subject().slice(0, scan);
  const senders = box.messages.sender().slice(0, scan);
  const dates = box.messages.dateReceived().slice(0, scan);
  const TERMS = {terms_js};
  const idx = [];
  for (let i = 0; i < ids.length; i++) {{
    if (!in_window(dates[i], sinceMs, untilMs)) continue;
    if (TERMS.length === 0 || TERMS.some(t =>
        (subjects[i] && subjects[i].toLowerCase().includes(t)) ||
        (senders[i] && senders[i].toLowerCase().includes(t)))) idx.push(i);
  }}
  const total = idx.length;
  const end = Math.min({offset} + {limit}, total);
  const out = [];
  for (let k = {offset}; k < end; k++) {{
    if (Date.now() - t0 > BUDGET_MS) {{ timedOut = true; break; }}
    const i = idx[k];
    const m = box.messages[i];
    const row = {{
      id: String(ids[i]),
      subject: subjects[i],
      from: senders[i],
      date: dates[i].toISOString().slice(0, 19) + 'Z',
    }};
    if ({snippets}) {{ try {{
      const c = String(m.content());
      let pos = 0;
      for (const t of TERMS) {{ const i = c.toLowerCase().indexOf(t); if (i >= 0) {{ pos = Math.max(0, i - 60); break; }} }}
      const s = c.slice(pos, pos + 140).trim();
      if (s) row.snippet = s;
    }} catch (e) {{}} }}
    out.push(row);
  }}
  const ret = {{total: total, results: out}};
  if (timedOut) ret.truncated = true;
  return ret;
}})()"#
    )
}

/// One script across ALL [`MailTargets`]: per target four bulk Apple
/// Events sliced to the per-call scan depth, JS filtering identical to
/// [`search_expr`], a `"Account/Mailbox"` folder tag per hit, a global
/// date-descending merge, pagination, and the [`GLOBAL_BUDGET_MS`] check
/// between targets. Unified mode scans the unified inbox under the tag
/// `Unified/Inbox` (per-message account attribution would cost one event
/// per message).
#[allow(clippy::too_many_arguments)]
fn search_multi_expr(
    targets: &MailTargets,
    query: &str,
    any_of: &[String],
    since: Option<&str>,
    until: Option<&str>,
    limit: u32,
    offset: u32,
    group: Option<MailGroupBy>,
    scan: u32,
    snippets: bool,
) -> String {
    let terms_js = terms_js(query, any_of);
    let window_clause = date_window_clause(since, until);
    let targets_js = match targets {
        MailTargets::Unified => "[{u: true}]".to_string(),
        MailTargets::Folders(pairs) => {
            let items: Vec<String> = pairs
                .iter()
                .map(|(a, b)| format!("{{a: {}, b: {}}}", js_str(a), js_str(b)))
                .collect();
            format!("[{}]", items.join(", "))
        }
    };
    let group_js = match group {
        Some(MailGroupBy::Sender) => "'sender'",
        Some(MailGroupBy::Subject) => "'subject'",
        None => "''",
    };
    format!(
        r#"(() => {{
  const M = Application('Mail');
  const BUDGET_MS = {GLOBAL_BUDGET_MS};
  const t0 = Date.now();
  const GROUP = {group_js};
  const SNIPPETS = {snippets};
  function in_window(d, lo, hi) {{
    if (!d) return lo === null && hi === null;
    const t = d.getTime();
    if (lo !== null && t < lo) return false;
    if (hi !== null && t >= hi) return false;
    return true;
  }}
  {window_clause}
  const TERMS = {terms_js};
  const targets = {targets_js};
  const merged = [];
  const scannedPerFolder = {{}};
  let truncated = false;
  for (const t of targets) {{
    if (Date.now() - t0 > BUDGET_MS) {{ truncated = true; break; }}
    let label, box;
    try {{
      if (t.u) {{
        box = M.inbox();
        label = 'Unified/Inbox';
      }} else {{
        label = t.a + '/' + t.b;
        // Resolve both specifiers to ARRAYS first: a stale account/mailbox
        // name yields an empty match, and indexing [0] on it would hand a
        // undefined `box` past this guard and kill the whole sweep with
        // AppleEvent -1728 on the first property fetch.
        const accs = M.accounts.whose({{name: t.a}})();
        const boxes = accs.length ? accs[0].mailboxes.whose({{name: t.b}})() : [];
        if (!boxes.length) continue;
        box = boxes[0];
      }}
    }} catch (e) {{ continue; }}
    const scan = Math.min(box.messages.length, {scan});
    scannedPerFolder[label] = scan;
    const ids = box.messages.id().slice(0, scan);
    const subjects = box.messages.subject().slice(0, scan);
    const senders = box.messages.sender().slice(0, scan);
    const dates = box.messages.dateReceived().slice(0, scan);
    for (let i = 0; i < ids.length; i++) {{
      if (!in_window(dates[i], sinceMs, untilMs)) continue;
      if (TERMS.length === 0 || TERMS.some(t =>
          (subjects[i] && subjects[i].toLowerCase().includes(t)) ||
          (senders[i] && senders[i].toLowerCase().includes(t)))) {{
        merged.push({{
          id: String(ids[i]),
          subject: subjects[i],
          from: senders[i],
          date: dates[i].toISOString().slice(0, 19) + 'Z',
          folder: label,
          ref: box.messages[i],
        }});
      }}
    }}
  }}
  merged.sort((x, y) => y.ms - x.ms);
  const total = merged.length;
  if (GROUP !== '') {{
    const normSub = s => String(s == null ? '' : s).toLowerCase().replace(/^(\s*(re|fwd?|fw)\s*(\[\d+\])?\s*:\s*)+/, '').replace(/\s+/g, ' ').trim();
    const addrOf = f => {{ const m = /<([^>]+)>/.exec(String(f || '')); return m ? m[1].trim().toLowerCase() : String(f || '').trim().toLowerCase(); }};
    const nameOf = f => {{ const a = addrOf(f); const m = /^\s*"?([^"<]+?)"?\s*</.exec(String(f || '')); return (m ? m[1].trim() : '') || a; }};
    const map = new Map();
    for (const e of merged) {{
      const key = GROUP === 'sender' ? addrOf(e.from) : normSub(e.subject);
      let g = map.get(key);
      if (!g) {{
        g = {{ key: key, name: nameOf(e.from), count: 0, first_ms: e.ms, first: e.date, last_ms: e.ms, last: e.date, latest_id: e.id, latest_ids: [], samples: [], folders: [] }};
        map.set(key, g);
      }}
      g.count++;
      if (e.ms < g.first_ms) {{ g.first_ms = e.ms; g.first = e.date; }}
      if (e.ms > g.last_ms) {{ g.last_ms = e.ms; g.last = e.date; g.latest_id = e.id; }}
      const ns = normSub(e.subject);
      if (g.latest_ids.length < 3) g.latest_ids.push(e.id);
      if (g.samples.length < 4 && !g.samples.some(x => normSub(x) === ns)) g.samples.push(String(e.subject == null ? '' : e.subject));
      if (g.folders.length < 3 && !g.folders.includes(e.folder)) g.folders.push(e.folder);
    }}
    let allGroups = Array.from(map.values());
    allGroups.sort((a, b) => (b.count - a.count) || (b.last_ms - a.last_ms));
    const totalGroups = allGroups.length;
    const endG = Math.min({offset} + {limit}, totalGroups);
    const page = [];
    for (let k2 = {offset}; k2 < endG; k2++) {{
      const g = allGroups[k2];
      page.push({{
        key: g.key,
        name: GROUP === 'sender' ? g.name : g.key,
        count: g.count,
        first: g.first,
        last: g.last,
        latest_id: g.latest_id,
        latest_ids: g.latest_ids,
        sample_subjects: g.samples,
        folders: g.folders,
      }});
    }}
    const ret = {{total: total, total_groups: totalGroups, groups: page, scanned_per_folder: scannedPerFolder}};
    if (truncated) ret.truncated = true;
    return ret;
  }}
  const end = Math.min({offset} + {limit}, total);
  const out = [];
  for (let k = {offset}; k < end; k++) {{
    if (Date.now() - t0 > BUDGET_MS) {{ truncated = true; break; }}
    const e = merged[k];
    const row = {{ id: e.id, subject: e.subject, from: e.from, date: e.date, folder: e.folder }};
    if (SNIPPETS) {{ try {{
      const c = String(e.ref.content());
      let pos = 0;
      for (const t of TERMS) {{ const i = c.toLowerCase().indexOf(t); if (i >= 0) {{ pos = Math.max(0, i - 60); break; }} }}
      const s = c.slice(pos, pos + 140).trim();
      if (s) row.snippet = s;
    }} catch (err) {{}} }}
    out.push(row);
  }}
  const ret = {{total: total, results: out, scanned_per_folder: scannedPerFolder}};
  if (truncated) ret.truncated = true;
  return ret;
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

/// Like [`mailboxes_expr`] but also reports each mailbox's newest message
/// date (`last_activity`, ISO or null) from a bounded tail sample of up to
/// 50 messages — best-effort, never fatal.
fn mailboxes_detailed_expr(account: Option<&str>) -> String {
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
      let lastActivity = null;
      try {{
        if (count > 0) {{
          const tail = Math.min(count, 50);
          const ds = mb.messages.dateReceived().slice(count - tail);
          let mx = 0;
          for (const d of ds) {{ const t = d.getTime(); if (t > mx) mx = t; }}
          if (mx > 0) lastActivity = new Date(mx).toISOString();
        }}
      }} catch (e) {{}}
      mailboxes.push({{
        account: acctName,
        name: mb.name(),
        count: count,
        last_activity: lastActivity,
      }});
    }}
  }}
  return {{mailboxes: mailboxes}};
}})()"#
    )
}

fn read_expr(id: &str, folder: Option<&str>) -> String {
    let id = js_str(id);
    // `folder` ("Account/Mailbox", as tagged on search rows) targets the
    // mailbox directly — one whose() per lookup. Without it: inbox first
    // (the common case), then a bounded sweep across every mailbox with
    // early exit on the first hit.
    let locate = match folder {
        Some(f) => match f.rsplit_once('/') {
            Some((a, b)) => format!(
                "const box = (() => {{ const acc = M.accounts.whose({{name: {}}})()[0]; \
                 if (!acc) throw new Error('account not found: {}'); \
                 return acc.mailboxes.whose({{name: {}}})()[0]; }})();",
                js_str(a),
                js_str(a),
                js_str(b)
            ),
            None => String::new(),
        },
        None => String::new(),
    };
    format!(
        r#"(() => {{
  const M = Application('Mail');
  const wanted = Number({id});
  const READ_MAX_CHARS = {READ_MAX_CHARS};
  const pick = (box) => {{
    const hits = box.messages.whose({{id: wanted}});
    return hits.length > 0 ? hits[0] : null;
  }};
  let m = null;
  {locate}
  if (typeof box !== 'undefined') {{
    m = pick(box);
    if (!m) throw new Error('message not found in folder: ' + {id});
  }} else {{
    m = pick(M.inbox());
    if (!m) {{
      outer: for (const acc of M.accounts() || []) {{
        const mbs = acc.mailboxes() || [];
        for (const mb of mbs) {{
          m = pick(mb);
          if (m) break outer;
        }}
      }}
    }}
    if (!m) throw new Error('message not found: ' + {id});
  }}
  let body = '';
  try {{ body = String(m.content()); }} catch (e) {{}}
  const truncated = body.length > READ_MAX_CHARS;
  if (truncated) body = body.slice(0, READ_MAX_CHARS);
  return {{
    id: String(m.id()),
    subject: m.subject(),
    from: m.sender(),
    date: m.dateReceived().toISOString(),
    body: body,
    body_truncated: truncated,
  }};
}})()"#
    )
}

/// Looks up a message by id exactly like [`read_expr`], runs Mail's native
/// `forward` verb addressed to `to`, attaches the optional comment to the
/// draft (best-effort), and sends it from the source message's account.
fn forward_expr(id: &str, to: &str, comment: Option<&str>) -> String {
    let id = js_str(id);
    let to = js_str(to);
    let comment_clause = match comment {
        Some(c) => format!("  try {{ fw.comment = {}; }} catch (e) {{}}\n", js_str(c)),
        None => String::new(),
    };
    format!(
        r#"(() => {{
  const M = Application('Mail');
  const wanted = Number({id});
  const hits = M.inbox().messages.whose({{id: wanted}});
  if (hits.length === 0) throw new Error('message not found: ' + {id});
  const fw = hits[0].forward({{to: {to}}});
{comment_clause}  fw.send();
  return {{status: 'forwarded'}};
}})()"#
    )
}

/// Looks up a message by id exactly like [`read_expr`], runs Mail's native
/// `reply` verb, replaces the draft content with `body`, and sends it from
/// the source message's account.
fn reply_expr(id: &str, body: &str) -> String {
    let id = js_str(id);
    let body = js_str(body);
    format!(
        r#"(() => {{
  const M = Application('Mail');
  const wanted = Number({id});
  const hits = M.inbox().messages.whose({{id: wanted}});
  if (hits.length === 0) throw new Error('message not found: ' + {id});
  const rp = hits[0].reply();
  rp.content = {body};
  rp.send();
  return {{status: 'replied'}};
}})()"#
    )
}

fn send_expr(to: &str, subject: &str, body: &str, from: Option<&str>) -> String {
    // Identity selection: case-insensitive match on account display name
    // or primary email address. No `from` means Mail's default account.
    let from_clause = match from {
        Some(f) => format!(
            r#"  const want = {}.toLowerCase();
  let acct = null;
  for (const a of M.accounts()) {{
    if (String(a.name()).toLowerCase() === want ||
        (a.emailAddresses().length > 0 &&
         String(a.emailAddresses()[0]).toLowerCase() === want)) {{
      acct = a;
      break;
    }}
  }}
  if (!acct) throw new Error('no matching account for from: ' + {});
  msg.account = acct;
"#,
            js_str(f),
            js_str(f)
        ),
        None => String::new(),
    };
    format!(
        r#"(() => {{
  const M = Application('Mail');
  const msg = M.OutgoingMessage({{subject: {}, content: {}}}).make();
  msg.to.push(M.ToRecipient({{address: {}}}).make());
{}  msg.send();
  return {{status: 'sent'}};
}})()"#,
        js_str(subject),
        js_str(body),
        js_str(to),
        from_clause,
    )
}

impl<T: AppleTransport> MailToolset<T> {
    /// Cached read: serve from `mail_bodies` when present (adds
    /// `"cached":true`, zero transport cost), else live read + write-through.
    /// Sync the given targets into `db_path`'s index (see mail_index::sync_targets).
    pub async fn sync_index(
        &mut self,
        handle: &personai_core::index::IndexHandle,
        targets: &MailTargets,
        full: bool,
        scan: u32,
    ) -> Result<String, AppleError> {
        crate::mail_index::sync_targets(&mut self.transport, handle, targets, full, scan).await
    }

    pub async fn read_cached(
        &mut self,
        id: &str,
        folder: Option<&str>,
        h: &personai_core::index::IndexHandle,
    ) -> Result<String, AppleError> {
        use crate::mail_index::{cache_get, cache_put};
        let index_err = |e: personai_core::index::IndexError| AppleError::Transport(e.to_string());
        let key = format!("{}|{id}", folder.unwrap_or("?"));
        if let Some(hit) = cache_get(h, &key).map_err(index_err)? {
            let mut v = hit;
            if let Some(o) = v.as_object_mut() {
                o.insert("cached".into(), serde_json::json!(true));
                o.entry("id".to_string())
                    .or_insert_with(|| serde_json::json!(id));
                o.entry("subject".to_string())
                    .or_insert_with(|| serde_json::json!(""));
                o.entry("from".to_string())
                    .or_insert_with(|| serde_json::json!(""));
                o.entry("date".to_string())
                    .or_insert_with(|| serde_json::json!(""));
                if let Some(t) = o.get_mut("body_truncated") {
                    let flag = t.as_i64().unwrap_or(0) != 0;
                    *t = serde_json::json!(flag);
                }
                o.remove("date_iso");
            }
            return Ok(v.to_string());
        }
        // Miss: live read, then write-through so repeat reads are free.
        let out = self.read(id, folder).await?;
        let parsed: Value = serde_json::from_str(&out)
            .map_err(|e| AppleError::Transport(format!("unreadable read payload: {e}")))?;
        cache_put(
            h,
            &key,
            parsed["subject"].as_str().unwrap_or_default(),
            parsed["from"].as_str().unwrap_or_default(),
            parsed["date"].as_str().unwrap_or_default(),
            parsed["body"].as_str().unwrap_or_default(),
            parsed["body_truncated"].as_bool().unwrap_or(false),
        )
        .map_err(index_err)?;
        // Flag honestly without a second DB round-trip or async recursion.
        let mut out_v = parsed;
        if let Some(o) = out_v.as_object_mut() {
            o.insert("cached".into(), serde_json::json!(true));
        }
        Ok(out_v.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_builder_escapes_user_text() {
        let s = search_expr(
            "O'Brien \"x\"",
            &[],
            Some("work"),
            None,
            None,
            None,
            20,
            0,
            5000,
            true,
        );
        assert!(s.contains(r#""o'brien \"x\"""#), "{s}");
        assert!(s.contains(r#"{name: "work"}"#), "{s}");
    }
    #[test]
    fn search_multi_builder_escapes_and_loops_targets() {
        let s = search_multi_expr(
            &MailTargets::Folders(vec![
                ("Wörk \"1\"".into(), "Box\\A".into()),
                ("B".into(), "C".into()),
            ]),
            "O'Brien",
            &[],
            Some("2026-08-01T00:00:00Z"),
            None,
            20,
            40,
            None,
            1000,
            true,
        );
        // Folder pairs are escaped verbatim (only the terms are lowercased).
        assert!(s.contains(r#"{a: "Wörk \"1\"", b: "Box\\A"}"#), "{s}");
        assert!(s.contains(r#"label = t.a + '/' + t.b;"#), "{s}");
        // Budget and per-target scan caps are baked into the script.
        assert!(s.contains("const BUDGET_MS = 25000;"), "{s}");
        assert!(s.contains("Math.min(box.messages.length, 1000)"), "{s}");
        // Terms are lowercased then escaped like search_expr.
        assert!(s.contains(r#"["o'brien"]"#), "{s}");

        let u = search_multi_expr(
            &MailTargets::Unified,
            "x",
            &[],
            None,
            None,
            10,
            0,
            None,
            5000,
            true,
        );
        assert!(u.contains("const targets = [{u: true}];"), "{u}");
        assert!(u.contains("'Unified/Inbox'"), "{u}");
    }

    #[test]
    fn forward_and_reply_builders_escape_user_text() {
        let f = forward_expr("42", "boss@x \"cc\"", Some("O'Brien \"note\""));
        assert!(f.contains(r#""boss@x \"cc\"""#), "{f}");
        assert!(f.contains(r#""O'Brien \"note\"""#), "{f}");
        assert!(f.contains(".forward({to:"), "{f}");
        assert!(f.contains("fw.comment = "), "{f}");
        assert!(
            !forward_expr("42", "boss@x", None).contains("comment"),
            "no comment clause when comment is None"
        );

        let r = reply_expr("7", "line\nbreak");
        assert!(r.contains("\"line\\nbreak\""), "{r}");
        assert!(r.contains(".reply()"), "{r}");
        assert!(r.contains("rp.content = "), "{r}");
    }

    #[test]
    fn send_builder_matches_from_by_name_or_email_only_when_given() {
        let with = send_expr("mom@x", "Hi", "Hello!", Some("Work"));
        // Case-insensitive identity match on name or primary email, then
        // the resolved account is pinned onto the outgoing message.
        assert!(with.contains(r#"want = "Work".toLowerCase()"#), "{with}");
        assert!(with.contains("msg.account = acct"), "{with}");
        assert!(with.contains("emailAddresses"), "{with}");
    }
}
