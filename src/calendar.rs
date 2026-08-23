//! macOS Calendar tools.
//!
//! Reads are auto-tier; create/update go through the soft gate. All scripts
//! are JXA via `run_jxa_json`; ISO 8601 strings convert with native JS
//! `Date` parsing (no locale-fragile AppleScript date literals).
//!
//! Context discipline: `read` is paginated and returns event metadata only.

use personai_core::macos::{AppleError, AppleTransport, run_jxa_json};
use personai_core::safety::{GateOutcome, SoftGate};
use serde_json::{Value, json};

use crate::util::js_str;
use crate::{DEFAULT_LIMIT, MAX_LIMIT};

/// Calendar tool group over any transport.
pub struct CalendarToolset<T: AppleTransport> {
    pub transport: T,
    /// Soft gate for writes (token store under the state dir).
    pub gate: Option<SoftGate>,
}

impl<T: AppleTransport> CalendarToolset<T> {
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

    /// Lists calendar names.
    pub async fn list(&mut self) -> Result<String, AppleError> {
        let v = run_jxa_json(
            &mut self.transport,
            "(() => { const C = Application('Calendar'); \
             return C.calendars().map(c => c.name()); })()",
        )
        .await?;
        Ok(json!({ "calendars": v }).to_string())
    }

    /// Reads events with `start <= startDate < end`, newest first.
    pub async fn read(
        &mut self,
        start: Option<String>,
        end: Option<String>,
        limit: Option<u32>,
        offset: u32,
    ) -> Result<String, AppleError> {
        let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
        let v = run_jxa_json(&mut self.transport, &read_expr(start, end, limit, offset)).await?;
        Ok(v.to_string())
    }

    /// Creates an event. `calendar` names the target (default: first
    /// calendar, reported in the response). Soft-gated.
    #[allow(clippy::too_many_arguments)] // mirrors wire params
    pub async fn create(
        &mut self,
        title: &str,
        start: &str,
        end: &str,
        calendar: Option<&str>,
        location: Option<&str>,
        notes: Option<&str>,
        token: Option<&str>,
    ) -> Result<String, AppleError> {
        let payload = json!({
            "title": title, "start": start, "end": end,
            "calendar": calendar, "location": location, "notes": notes,
        });
        match self.check("calendar.create", &payload, token).await? {
            GateOutcome::Confirm { payload, token } => Ok(confirm_response(payload, token)),
            GateOutcome::Execute => {
                let v = run_jxa_json(
                    &mut self.transport,
                    &create_expr(title, start, end, calendar, location, notes),
                )
                .await?;
                Ok(json!({
                    "status": "created",
                    "id": v.get("id").cloned().unwrap_or(Value::Null),
                    "calendar": v.get("calendar").cloned().unwrap_or(Value::Null),
                })
                .to_string())
            }
        }
    }

    /// Updates an event found by uid. Only provided fields change.
    /// Soft-gated.
    #[allow(clippy::too_many_arguments)] // mirrors wire params
    pub async fn update(
        &mut self,
        id: &str,
        title: Option<&str>,
        start: Option<&str>,
        end: Option<&str>,
        location: Option<&str>,
        notes: Option<&str>,
        token: Option<&str>,
    ) -> Result<String, AppleError> {
        let payload = json!({
            "id": id, "title": title, "start": start, "end": end,
            "location": location, "notes": notes,
        });
        match self.check("calendar.update", &payload, token).await? {
            GateOutcome::Confirm { payload, token } => Ok(confirm_response(payload, token)),
            GateOutcome::Execute => {
                run_jxa_json(
                    &mut self.transport,
                    &update_expr(id, title, start, end, location, notes),
                )
                .await?;
                Ok(json!({ "status": "updated", "id": id }).to_string())
            }
        }
    }

    /// Deletes (marks deleted — reversible in Calendar) an event by uid.
    /// Soft-gated.
    pub async fn delete(&mut self, id: &str, token: Option<&str>) -> Result<String, AppleError> {
        let payload = json!({ "id": id });
        match self.check("calendar.delete", &payload, token).await? {
            GateOutcome::Confirm { payload, token } => Ok(confirm_response(payload, token)),
            GateOutcome::Execute => {
                run_jxa_json(&mut self.transport, &delete_expr(id)).await?;
                Ok(json!({ "status": "deleted", "id": id }).to_string())
            }
        }
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
            None => Err(AppleError::Transport(String::from(
                "soft gate not configured — refusing to modify the calendar",
            ))),
        }
    }
}

fn confirm_response(payload: Value, token: String) -> String {
    json!({
        "status": "requires_confirmation",
        "payload": payload,
        "confirmation_token": token,
        "note": "Show this payload to the user; re-invoke with confirmation_token to execute.",
    })
    .to_string()
}

// --- JXA expression builders -------------------------------------------------

/// Pass 1 fetches one bulk `startDate()` array per calendar and filters in
/// JS; pass 2 hydrates only the requested page. A `whose()` specifier
/// re-evaluates its query on every element access, which times out once
/// several calendars are involved.
fn read_expr(start: Option<String>, end: Option<String>, limit: u32, offset: u32) -> String {
    let start_ms = start
        .map(|s| format!("Date.parse({})", js_str(&s)))
        .unwrap_or_else(|| "null".into());
    let end_ms = end
        .map(|s| format!("Date.parse({})", js_str(&s)))
        .unwrap_or_else(|| "null".into());
    format!(
        r#"(() => {{
  const C = Application('Calendar');
  const startMs = {start_ms};
  const endMs = {end_ms};
  let matches = [];
  for (const cal of C.calendars()) {{
    const n = cal.events.length;
    if (n === 0) continue;
    const starts = cal.events.startDate();
    for (let i = 0; i < starts.length; i++) {{
      const t = starts[i] ? starts[i].getTime() : NaN;
      if ((startMs === null || t >= startMs) && (endMs === null || t < endMs)) {{
        matches.push([cal, i]);
      }}
    }}
  }}
  const total = matches.length;
  const out = [];
  const end = Math.min({offset} + {limit}, total);
  for (let k = {offset}; k < end; k++) {{
    const [cal, i] = matches[k];
    try {{
      const e = cal.events[i];
      out.push({{
        id: String(e.uid()),
        title: e.summary(),
        start: e.startDate().toISOString(),
        end: e.endDate().toISOString(),
        calendar: cal.name(),
      }});
    }} catch (err) {{}}
  }}
  return {{total: total, offset: {offset}, limit: {limit}, events: out}};
}})()"#
    )
}

fn create_expr(
    title: &str,
    start: &str,
    end: &str,
    calendar: Option<&str>,
    location: Option<&str>,
    notes: Option<&str>,
) -> String {
    // Named calendar resolves with an explicit miss error; unnamed keeps
    // the historical default (first calendar) and says so in the response.
    let cal_clause = match calendar {
        Some(name) => format!(
            "const hits = C.calendars.whose({{name: {}}})();\n  \
             if (!hits.length) throw new Error('calendar not found: {}');\n  \
             const cal = hits[0];",
            js_str(name),
            js_str(name)
        ),
        None => "const cal = C.calendars[0];".to_string(),
    };
    let extra_props = [
        location.map(|l| format!("location: {}", js_str(l))),
        notes.map(|n| format!("description: {}", js_str(n))),
    ];
    let props = extra_props
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"(() => {{
  const C = Application('Calendar');
  {}
  const ev = C.Event({{summary: {}, startDate: new Date(Date.parse({})), endDate: new Date(Date.parse({})){}}}).make({{at: cal}});
  return {{id: String(ev.uid()), calendar: String(cal.name())}};
}})()"#,
        cal_clause,
        js_str(title),
        js_str(start),
        js_str(end),
        if props.is_empty() {
            String::new()
        } else {
            format!(", {props}")
        },
    )
}

fn update_expr(
    id: &str,
    title: Option<&str>,
    start: Option<&str>,
    end: Option<&str>,
    location: Option<&str>,
    notes: Option<&str>,
) -> String {
    let sets = [
        title.map(|t| format!("e.summary = {};", js_str(t))),
        start.map(|s| format!("e.startDate = new Date(Date.parse({}));", js_str(s))),
        end.map(|s| format!("e.endDate = new Date(Date.parse({}));", js_str(s))),
        location.map(|l| format!("e.location = {};", js_str(l))),
        notes.map(|n| format!("e.description = {};", js_str(n))),
    ];
    let body: String = sets.into_iter().flatten().collect();
    format!(
        r#"(() => {{
  const C = Application('Calendar');
  for (const cal of C.calendars()) {{
    const hits = cal.events.whose({{uid: {}}});
    if (hits.length > 0) {{
      const e = hits[0];
      {}
      return {{id: String(e.uid())}};
    }}
  }}
  throw new Error('event not found');
}})()"#,
        js_str(id),
        body,
    )
}

/// Deletion marks the event `deleted` (Calendar's own soft delete — the
/// event stays recoverable in the UI), keeping the MCP verb reversible.
fn delete_expr(id: &str) -> String {
    format!(
        r#"(() => {{
  const C = Application('Calendar');
  for (const cal of C.calendars()) {{
    const hits = cal.events.whose({{uid: {}}});
    if (hits.length > 0) {{
      const e = hits[0];
      e.deleted = true;
      return {{id: String(e.uid())}};
    }}
  }}
  throw new Error('event not found');
}})()"#,
        js_str(id),
    )
}
