//! macOS notification posting.
//!
//! Auto-tier (no gate): posts a user-facing notification through Standard
//! Additions' `displayNotification`.

use personai_core::macos::{AppleError, AppleTransport, run_jxa_json};
use serde_json::json;

use crate::util::js_str;

/// Posts local notifications.
pub struct NotificationsToolset<T: AppleTransport> {
    pub transport: T,
}

impl<T: AppleTransport> NotificationsToolset<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    /// Shows a notification banner owned by this process.
    pub async fn post(
        &mut self,
        title: &str,
        message: &str,
        subtitle: Option<&str>,
    ) -> Result<String, AppleError> {
        let subtitle_clause = match subtitle {
            Some(s) => format!(", subtitle: {}", js_str(s)),
            None => String::new(),
        };
        let expr = format!(
            r#"(() => {{
  const app = Application.currentApplication();
  app.includeStandardAdditions = true;
  app.displayNotification({}, {{withTitle: {}{subtitle_clause}}});
  return {{status: 'posted'}};
}})()"#,
            js_str(message),
            js_str(title),
        );
        run_jxa_json(&mut self.transport, &expr).await?;
        Ok(json!({ "status": "posted", "title": title }).to_string())
    }
}
