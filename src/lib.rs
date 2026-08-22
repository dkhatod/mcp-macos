//! mcp-macos library: five self-contained Apple tool groups behind one MCP
//! server.
//!
//! Layout: each tool group lives in its own module (`mail`, `messages`, …)
//! as a `*Toolset<T: AppleTransport>` that is fully testable against
//! [`personai_core::macos::MockTransport`]. The MCP surface is
//! [`MacosServer`] — thin `#[tool]` methods that lock [`ServerState`] and
//! delegate to the toolsets.
//!
//! See `docs/development.md` for how to add a tool group.

use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::schemars::JsonSchema;
use rmcp::tool;
use rmcp::tool_router;
use serde::Deserialize;
use tokio::sync::Mutex;

pub mod calendar;
pub mod clipboard;
pub mod mail;
pub mod messages;
pub mod notifications;
pub mod util;

use calendar::CalendarToolset;
use clipboard::ClipboardToolset;

use mail::MailToolset;
use messages::MessagesToolset;
use notifications::NotificationsToolset;
use personai_core::macos::{AppleError, RealTransport};

/// Default page size for list/search tools.
pub(crate) const DEFAULT_LIMIT: u32 = 20;
/// Hard maximum page size (context discipline: no unbounded blobs).
pub(crate) const MAX_LIMIT: u32 = 100;

/// Mutable server state shared by all tools.
///
/// The MCP handler methods take `&self`; everything that must mutate
/// (transports, gates) lives here behind an async mutex, because tool bodies
/// await subprocesses while holding the lock. Each field is one tool group;
/// groups never touch each other.
pub struct ServerState {
    mail: MailToolset<RealTransport>,
    messages: MessagesToolset<RealTransport>,
    calendar: CalendarToolset<RealTransport>,
    notifications: NotificationsToolset<RealTransport>,
    clipboard: ClipboardToolset,
}

/// The MCP server handle passed to every tool method.
pub struct MacosServer {
    pub state_dir: std::path::PathBuf,
    inner: Arc<Mutex<ServerState>>,
}

#[tool_router(server_handler)]
impl MacosServer {
    /// Build a server rooted at `state_dir`; each gated group keeps its own
    /// token store under it (`tokens.mail.json`, `tokens.messages.json`, …).
    pub fn new(state_dir: std::path::PathBuf) -> Self {
        let gated = |store: &str| state_dir.join(store);
        let state = ServerState {
            mail: MailToolset::with_gate(RealTransport, gated("tokens.mail.json")).unwrap_or_else(
                |e| {
                    eprintln!("mail gate unavailable ({e}); mail_send will refuse");
                    MailToolset::new(RealTransport)
                },
            ),
            messages: MessagesToolset::with_gate(RealTransport, gated("tokens.messages.json"))
                .unwrap_or_else(|e| {
                    eprintln!("messages gate unavailable ({e}); messages_send will refuse");
                    MessagesToolset::new(RealTransport)
                }),
            calendar: CalendarToolset::with_gate(RealTransport, gated("tokens.calendar.json"))
                .unwrap_or_else(|e| {
                    eprintln!("calendar gate unavailable ({e}); writes will refuse");
                    CalendarToolset::new(RealTransport)
                }),
            notifications: NotificationsToolset::new(RealTransport),
            clipboard: ClipboardToolset::new(),
        };
        Self {
            state_dir,
            inner: Arc::new(Mutex::new(state)),
        }
    }

    // --- Mail ---------------------------------------------------------------

    #[tool(description = "List Mail account names configured on this Mac.")]
    async fn mail_list_accounts(&self) -> String {
        let mut st = self.inner.lock().await;
        json_result(st.mail.list_accounts().await)
    }

    #[tool(
        description = "Search Mail. Returns metadata only (id, subject, from, date, snippet), paginated as {total, offset, limit, results}. Use mail_read for full content."
    )]
    async fn mail_search(&self, Parameters(p): Parameters<MailSearchParams>) -> String {
        let mut st = self.inner.lock().await;
        json_result(
            st.mail
                .search(
                    &p.query,
                    p.account.as_deref(),
                    p.since.as_deref(),
                    p.limit,
                    p.offset.unwrap_or(0),
                )
                .await,
        )
    }

    #[tool(description = "Read one full Mail message by id (an id from mail_search results).")]
    async fn mail_read(&self, Parameters(p): Parameters<MailReadParams>) -> String {
        let mut st = self.inner.lock().await;
        json_result(st.mail.read(&p.id).await)
    }

    #[tool(
        description = "Send an email via Mail. Soft-gated: the first call returns status requires_confirmation with a confirmation_token and does NOT send; re-invoke with confirmation_token to execute. Tokens are single-use and expire in 5 minutes."
    )]
    async fn mail_send(&self, Parameters(p): Parameters<MailSendParams>) -> String {
        let mut st = self.inner.lock().await;
        json_result(
            st.mail
                .send(&p.to, &p.subject, &p.body, p.confirmation_token.as_deref())
                .await,
        )
    }

    // --- Messages ------------------------------------------------------------

    #[tool(
        description = "Read recent iMessage/SMS history, newest first. Returns {total, offset, limit, messages:[{from, direction, text, date}]}. chat optionally filters by chat id, display name, or participant handle."
    )]
    async fn messages_read(&self, Parameters(p): Parameters<MessagesReadParams>) -> String {
        let mut st = self.inner.lock().await;
        json_result(
            st.messages
                .read(p.chat, p.limit, p.offset.unwrap_or(0))
                .await,
        )
    }

    #[tool(
        description = "Send an iMessage/SMS. Soft-gated: the first call returns status requires_confirmation with a confirmation_token and does NOT send; re-invoke with confirmation_token to execute. Tokens are single-use and expire in 5 minutes."
    )]
    async fn messages_send(&self, Parameters(p): Parameters<MessagesSendParams>) -> String {
        let mut st = self.inner.lock().await;
        json_result(
            st.messages
                .send(&p.to, &p.body, p.confirmation_token.as_deref())
                .await,
        )
    }

    // --- Notifications -------------------------------------------------------

    #[tool(
        description = "Post a local macOS notification banner (title, optional subtitle, message)."
    )]
    async fn notifications_post(&self, Parameters(p): Parameters<NotificationParams>) -> String {
        let mut st = self.inner.lock().await;
        json_result(
            st.notifications
                .post(&p.title, &p.message, p.subtitle.as_deref())
                .await,
        )
    }

    // --- Calendar ------------------------------------------------------------

    #[tool(description = "List calendar names on this Mac.")]
    async fn calendar_list(&self) -> String {
        let mut st = self.inner.lock().await;
        json_result(st.calendar.list().await)
    }

    #[tool(
        description = "Read events with start date in [start, end) (ISO 8601). Returns {total, offset, limit, events:[{id, title, start, end, calendar}]}."
    )]
    async fn calendar_read(&self, Parameters(p): Parameters<CalendarReadParams>) -> String {
        let mut st = self.inner.lock().await;
        json_result(
            st.calendar
                .read(Some(p.start), Some(p.end), p.limit, p.offset.unwrap_or(0))
                .await,
        )
    }

    #[tool(
        description = "Create a calendar event (first calendar). Soft-gated: re-invoke with confirmation_token to execute. Tokens are single-use, 5-minute TTL."
    )]
    async fn calendar_create(&self, Parameters(p): Parameters<CalendarCreateParams>) -> String {
        let mut st = self.inner.lock().await;
        json_result(
            st.calendar
                .create(&p.title, &p.start, &p.end, p.confirmation_token.as_deref())
                .await,
        )
    }

    #[tool(
        description = "Update an event by id (uid from calendar_read); only provided fields change. Soft-gated."
    )]
    async fn calendar_update(&self, Parameters(p): Parameters<CalendarUpdateParams>) -> String {
        let mut st = self.inner.lock().await;
        json_result(
            st.calendar
                .update(
                    &p.id,
                    p.title.as_deref(),
                    p.start.as_deref(),
                    p.end.as_deref(),
                    p.confirmation_token.as_deref(),
                )
                .await,
        )
    }

    // --- Clipboard -----------------------------------------------------------

    #[tool(description = "Read UTF-8 text from the macOS clipboard.")]
    async fn clipboard_get(&self) -> String {
        let st = self.inner.lock().await;
        match st.clipboard.get() {
            Ok(text) => serde_json::json!({ "text": text }).to_string(),
            Err(e) => serde_json::json!({ "error": e.to_string() }).to_string(),
        }
    }

    #[tool(description = "Write UTF-8 text to the macOS clipboard.")]
    async fn clipboard_set(&self, Parameters(p): Parameters<ClipboardSetParams>) -> String {
        let st = self.inner.lock().await;
        match st.clipboard.set(&p.text) {
            Ok(()) => serde_json::json!({ "status": "set" }).to_string(),
            Err(e) => serde_json::json!({ "error": e.to_string() }).to_string(),
        }
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct CalendarReadParams {
    /// Range start (ISO 8601), inclusive.
    pub start: String,
    /// Range end (ISO 8601), exclusive.
    pub end: String,
    /// Page size (default 20, max 100).
    #[serde(default)]
    pub limit: Option<u32>,
    /// Page offset (default 0).
    #[serde(default)]
    pub offset: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
pub struct CalendarCreateParams {
    /// Event title.
    pub title: String,
    /// Start time (ISO 8601).
    pub start: String,
    /// End time (ISO 8601).
    pub end: String,
    /// Token from a previous requires_confirmation response.
    #[serde(default)]
    pub confirmation_token: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct CalendarUpdateParams {
    /// Event uid from calendar_read.
    pub id: String,
    /// New title (optional).
    #[serde(default)]
    pub title: Option<String>,
    /// New start time (optional, ISO 8601).
    #[serde(default)]
    pub start: Option<String>,
    /// New end time (optional, ISO 8601).
    #[serde(default)]
    pub end: Option<String>,
    /// Token from a previous requires_confirmation response.
    #[serde(default)]
    pub confirmation_token: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ClipboardSetParams {
    /// Text to place on the clipboard.
    pub text: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct MessagesReadParams {
    /// Optional chat id, display name, or participant handle filter.
    #[serde(default)]
    pub chat: Option<String>,
    /// Page size (default 20, max 100).
    #[serde(default)]
    pub limit: Option<u32>,
    /// Page offset (default 0).
    #[serde(default)]
    pub offset: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
pub struct MessagesSendParams {
    /// Participant handle (phone number or email).
    pub to: String,
    /// Message body.
    pub body: String,
    /// Token from a previous requires_confirmation response.
    #[serde(default)]
    pub confirmation_token: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct NotificationParams {
    /// Banner title.
    pub title: String,
    /// Banner message body.
    pub message: String,
    /// Optional subtitle.
    #[serde(default)]
    pub subtitle: Option<String>,
}

// --- Tool parameter schemas ---------------------------------------------

#[derive(Deserialize, JsonSchema)]
pub struct MailSearchParams {
    /// Case-insensitive text matched against subject or sender.
    pub query: String,
    /// Optional Mail account name to search within.
    #[serde(default)]
    pub account: Option<String>,
    /// Optional ISO 8601 date; only messages received after this instant.
    #[serde(default)]
    pub since: Option<String>,
    /// Page size (default 20, max 100).
    #[serde(default)]
    pub limit: Option<u32>,
    /// Page offset (default 0).
    #[serde(default)]
    pub offset: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
pub struct MailReadParams {
    /// Message id from a mail_search result.
    pub id: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct MailSendParams {
    /// Recipient address.
    pub to: String,
    /// Subject line.
    pub subject: String,
    /// Plain-text body.
    pub body: String,
    /// Token from a previous requires_confirmation response.
    #[serde(default)]
    pub confirmation_token: Option<String>,
}

// --- Helpers -------------------------------------------------------------

/// Serializes a toolset result into the single-JSON-string response every
/// tool returns; errors become `{"error": ...}` payloads so the calling
/// agent always receives structured output.
fn json_result(r: Result<String, AppleError>) -> String {
    match r {
        Ok(s) => s,
        Err(e) => serde_json::json!({ "error": e.to_string() }).to_string(),
    }
}
