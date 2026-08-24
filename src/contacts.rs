//! Contacts search (read-only).
//!
//! Bulk `name()/organization()/emails.value()` fetches filtered in JS (the
//! same pattern that keeps mail search off the per-item `whose()` trap);
//! per-person phone hydration happens only for the requested page.
//! Auto-tier: reads never mutate and only page-sized detail leaves the app.

use personai_core::macos::{AppleError, AppleTransport, run_jxa_json};

use crate::util::js_str;
use crate::{DEFAULT_LIMIT, MAX_LIMIT};

/// Contacts read group over any transport.
pub struct ContactsToolset<T: AppleTransport> {
    pub transport: T,
}

impl<T: AppleTransport> ContactsToolset<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    /// Case-insensitive substring search over name, organization, and email
    /// addresses. Empty query = directory census (paginated metadata).
    pub async fn search(
        &mut self,
        query: &str,
        limit: Option<u32>,
        offset: u32,
    ) -> Result<String, AppleError> {
        let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
        let v = run_jxa_json(&mut self.transport, &search_expr(query, limit, offset)).await?;
        Ok(crate::util::unwrap_string_payload(v)?.to_string())
    }
}

// --- JXA expression builders -------------------------------------------------

/// Scan cap: bulk arrays are fetched whole anyway (one Apple Event each);
/// this bounds JS post-processing exactly like [`crate::mail`] does.
const SCAN_MAX: u32 = 20_000;

fn search_expr(query: &str, limit: u32, offset: u32) -> String {
    format!(
        r#"(() => {{
  const C = Application('Contacts');
  const t0 = Date.now();
  const BUDGET_MS = 20000;
  const q = {};
  const people = C.people;
  const scan = Math.min(people.length, {SCAN_MAX});
  const ids = people.id().slice(0, scan);
  const names = people.name().slice(0, scan);
  let orgs = null;
  try {{ orgs = people.organization().slice(0, scan); }} catch (e) {{}}
  let emailsNested = null;
  try {{ emailsNested = people.emails.value().slice(0, scan); }} catch (e) {{}}
  const idx = [];
  for (let i = 0; i < scan; i++) {{
    let hay = String(names[i] || '') + ' ' + String(orgs ? orgs[i] : '');
    if (emailsNested && emailsNested[i]) hay += ' ' + emailsNested[i].join(' ');
    hay = hay.toLowerCase();
    if (q === '' || hay.includes(q)) idx.push(i);
  }}
  const total = idx.length;
  const end = Math.min({offset} + {limit}, total);
  const out = [];
  for (let k = {offset}; k < end; k++) {{
    if (Date.now() - t0 > BUDGET_MS) break;
    const i = idx[k];
    let phones = [];
    try {{ phones = people[i].phones.value().map(p => String(p)); }} catch (e) {{}}
    const c = {{ id: String(ids[i]), name: names[i] }};
    if (orgs && orgs[i]) c.organization = orgs[i];
    if (emailsNested && emailsNested[i] && emailsNested[i].length) c.emails = emailsNested[i].map(String);
    if (phones.length) c.phones = phones;
    out.push(c);
  }}
  const rows = out.map(c => JSON.stringify(c)).join(',\n');
  return '{{"total":' + total + ',"contacts":[\n' + rows + '\n]}}';
}})()"#,
        js_str(&query.to_lowercase()),
    )
}
