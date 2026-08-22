//! TCC permission doctor (spec §11.1).
//!
//! [`check`] runs a minimal read-only probe against each AppleEvent target
//! and reports per-app status plus the exact fix for missing grants. The
//! first run may surface a TCC consent prompt — that prompt IS the fix
//! flow; after granting, re-run `permissions_check`.

use personai_core::macos::{AppleError, AppleTransport, run_jxa_json};
use serde_json::json;

/// Probe scripts: cheapest possible read per app.
const PROBES: &[(&str, &str)] = &[
    (
        "Mail",
        "(() => { return Application('Mail').accounts().length; })()",
    ),
    (
        "Calendar",
        "(() => { return Application('Calendar').calendars().length; })()",
    ),
    (
        "Messages",
        "(() => { return Application('Messages').accounts.length; })()",
    ),
];

/// Runs all probes. Response shape:
/// `{ok: bool, permissions: [{app, ok, fix?}]}` — `fix` present only on
/// denied entries, carrying actionable System Settings guidance.
pub async fn check<T: AppleTransport>(t: &mut T) -> Result<String, AppleError> {
    let mut entries = Vec::with_capacity(PROBES.len());
    let mut all_ok = true;
    for (app, script) in PROBES {
        match run_jxa_json(t, script).await {
            Ok(_) => entries.push(json!({ "app": app, "ok": true })),
            Err(e @ AppleError::PermissionDenied(_)) => {
                all_ok = false;
                entries.push(json!({
                    "app": app,
                    "ok": false,
                    "error": e.to_string(),
                    "fix": e.hint(),
                }));
            }
            Err(e) => {
                all_ok = false;
                entries.push(json!({
                    "app": app,
                    "ok": false,
                    "error": e.to_string(),
                    "fix": e.hint(),
                }));
            }
        }
    }
    Ok(json!({ "ok": all_ok, "permissions": entries }).to_string())
}
