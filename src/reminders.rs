//! Reminders tools.
//!
//! Reads are auto-tier and paginated; create/complete go through the soft
//! gate like every other write group. All scripts are JXA via
//! `run_jxa_json`; due dates are ISO 8601 parsed with native JS `Date`.

use personai_core::macos::{AppleError, AppleTransport, run_jxa_json};
use serde_json::{Value, json};

use crate::util::js_str;
use crate::{DEFAULT_LIMIT, MAX_LIMIT};

/// Reminders tool group over any transport.
pub struct RemindersToolset<T: AppleTransport> {
    pub transport: T,
    /// Soft gate for create/complete (token store under the state dir).
    pub gate: Option<SoftGate>,
}

use personai_core::safety::{GateOutcome, SoftGate};

impl<T: AppleTransport> RemindersToolset<T> {
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

    async fn check(
        &mut self,
        action: &'static str,
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

    /// Lists reminder list names. Auto-tier.
    pub async fn list_lists(&mut self) -> Result<String, AppleError> {
        let v = run_jxa_json(&mut self.transport, LISTS_EXPR).await?;
        Ok(v.to_string())
    }

    /// Reads reminders (open items unless `include_completed`), newest due
    /// first within the matched set. Auto-tier.
    pub async fn read(
        &mut self,
        list: Option<String>,
        include_completed: bool,
        limit: Option<u32>,
        offset: u32,
    ) -> Result<String, AppleError> {
        let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
        let v = run_jxa_json(
            &mut self.transport,
            &read_expr(list.as_deref(), include_completed, limit, offset),
        )
        .await?;
        Ok(crate::util::unwrap_string_payload(v)?.to_string())
    }

    /// Creates a reminder. Soft-gated.
    pub async fn create(
        &mut self,
        name: &str,
        list: Option<&str>,
        due: Option<&str>,
        notes: Option<&str>,
        token: Option<&str>,
    ) -> Result<String, AppleError> {
        let payload = json!({
            "action": "reminders.create",
            "name": name,
            "list": list,
            "due": due,
            "notes": notes,
        });
        match self.check("reminders.create", &payload, token).await? {
            GateOutcome::Confirm { payload, token } => Ok(json!({
                "status": "requires_confirmation",
                "payload": payload,
                "confirmation_token": token,
                "note": "re-invoke with confirmation_token",
            })
            .to_string()),
            GateOutcome::Execute => {
                run_jxa_json(&mut self.transport, &create_expr(name, list, due, notes)).await?;
                Ok(json!({ "status": "created", "name": name }).to_string())
            }
        }
    }

    /// Marks a reminder completed by id. Soft-gated (mutation).
    pub async fn complete(&mut self, id: &str, token: Option<&str>) -> Result<String, AppleError> {
        let payload = json!({ "action": "reminders.complete", "id": id });
        match self.check("reminders.complete", &payload, token).await? {
            GateOutcome::Confirm { payload, token } => Ok(json!({
                "status": "requires_confirmation",
                "payload": payload,
                "confirmation_token": token,
            })
            .to_string()),
            GateOutcome::Execute => {
                run_jxa_json(&mut self.transport, &complete_expr(id)).await?;
                Ok(json!({ "status": "completed", "id": id }).to_string())
            }
        }
    }
}

// --- JXA expression builders -------------------------------------------------

const LISTS_EXPR: &str = r#"(() => { const R = Application('Reminders'); return {lists: R.lists().map(l => ({name: l.name()}))}; })()"#;

/// Container clause shared by read/create: a named list resolves through
/// `whose` with an explicit miss error; otherwise the app-wide pool.
fn container_clause(list: Option<&str>) -> String {
    match list {
        Some(name) => format!(
            "const lists = R.lists.whose({{name: {}}})();\n  if (!lists.length) throw new Error('list not found: {}');\n  const box = lists[0].reminders;",
            js_str(name),
            js_str(name)
        ),
        None => "const box = R.reminders;".to_string(),
    }
}

fn read_expr(list: Option<&str>, include_completed: bool, limit: u32, offset: u32) -> String {
    format!(
        r#"(() => {{
  const R = Application('Reminders');
  {}
  const scan = Math.min(box.length, 5000);
  const ids = box.id().slice(0, scan);
  const names = box.name().slice(0, scan);
  let done = null;
  try {{ done = box.completed().slice(0, scan); }} catch (e) {{}}
  const wantDone = {include_completed};
  const idx = [];
  for (let i = 0; i < scan; i++) {{
    if (!wantDone && done && done[i]) continue;
    idx.push(i);
  }}
  const total = idx.length;
  const end = Math.min({offset} + {limit}, total);
  const out = [];
  for (let k = {offset}; k < end; k++) {{
    const i = idx[k];
    let due = null;
    try {{ const d = box[i].dueDate(); due = d ? d.toISOString() : null; }} catch (e) {{}}
    let body = '';
    try {{ body = String(box[i].body() || ''); }} catch (e) {{}}
    let priority = 0;
    try {{ priority = Number(box[i].priority()) || 0; }} catch (e) {{}}
    const r = {{ id: String(ids[i]), name: names[i] }};
    if (due) r.due = due;
    if (body) r.body = body;
    if (priority) r.priority = priority;
    if (done) r.completed = !!done[i];
    out.push(r);
  }}
  const rows = out.map(r => JSON.stringify(r)).join(',\n');
  return '{{"total":' + total + ',"reminders":[\n' + rows + '\n]}}';
}})()"#,
        container_clause(list),
    )
}

fn create_expr(name: &str, list: Option<&str>, due: Option<&str>, notes: Option<&str>) -> String {
    let due_clause = match due {
        Some(iso) => format!(
            "\n  try {{ r.dueDate = new Date({}); }} catch (e) {{}}",
            js_str(iso)
        ),
        None => String::new(),
    };
    let notes_clause = match notes {
        Some(n) if !n.is_empty() => format!("\n  try {{ r.body = {}; }} catch (e) {{}}", js_str(n)),
        _ => String::new(),
    };
    format!(
        r#"(() => {{
  const R = Application('Reminders');
  {}
  const r = R.Reminder({{name: {}}});
  box.push(r);{}
{}  return {{status: 'created', id: String(r.id())}};
}})()"#,
        container_clause(list),
        js_str(name),
        due_clause,
        notes_clause,
    )
}

fn complete_expr(id: &str) -> String {
    format!(
        r#"(() => {{
  const R = Application('Reminders');
  const hits = R.reminders.whose({{id: {}}})();
  if (!hits.length) throw new Error('reminder not found: {}');
  hits[0].completed = true;
  return {{status: 'completed', id: String(hits[0].id())}};
}})()"#,
        js_str(id),
        js_str(id),
    )
}
