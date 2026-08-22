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
use rmcp::tool_handler;
use rmcp::tool_router;
use serde::Deserialize;
use tokio::sync::Mutex;

pub mod calendar;
pub mod clipboard;
pub mod mail;
pub mod messages;
pub mod notifications;
pub mod permissions;
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

/// Consumer-side tool-set trimming (spec §11.1): a client can load only the
/// groups it needs via `--tools mail,calendar`. `permissions_check` is
/// always available.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EnabledTools {
    pub mail: bool,
    pub messages: bool,
    pub calendar: bool,
    pub notifications: bool,
    pub clipboard: bool,
}

impl EnabledTools {
    pub fn all() -> Self {
        Self {
            mail: true,
            messages: true,
            calendar: true,
            notifications: true,
            clipboard: true,
        }
    }

    /// Parses a comma-separated group list; unknown names are errors.
    pub fn parse(csv: &str) -> Result<Self, String> {
        let mut e = Self {
            mail: false,
            messages: false,
            calendar: false,
            notifications: false,
            clipboard: false,
        };
        for name in csv.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match name {
                "mail" => e.mail = true,
                "messages" => e.messages = true,
                "calendar" => e.calendar = true,
                "notifications" => e.notifications = true,
                "clipboard" => e.clipboard = true,
                other => {
                    return Err(format!(
                        "unknown tool group '{other}' (valid: mail, messages, calendar, notifications, clipboard)"
                    ));
                }
            }
        }
        Ok(e)
    }

    fn allows(&self, tool_name: &str) -> bool {
        let on = if tool_name.starts_with("mail_") {
            self.mail
        } else if tool_name.starts_with("messages_") {
            self.messages
        } else if tool_name.starts_with("calendar_") {
            self.calendar
        } else if tool_name.starts_with("notifications_") {
            self.notifications
        } else if tool_name.starts_with("clipboard_") {
            self.clipboard
        } else {
            true // permissions_check and future ungrouped tools
        };
        on || tool_name == "permissions_check"
    }
}

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
    enabled: EnabledTools,
    inner: Arc<Mutex<ServerState>>,
}

#[tool_router]
impl MacosServer {
    /// Build a server rooted at `state_dir` with every group enabled; each
    /// gated group keeps its own token store under it (`tokens.mail.json`,
    /// `tokens.messages.json`, …).
    pub fn new(state_dir: std::path::PathBuf) -> Self {
        Self::new_with_tools(state_dir, EnabledTools::all())
    }

    /// Build a server exposing only the enabled groups (spec §11.1).
    pub fn new_with_tools(state_dir: std::path::PathBuf, enabled: EnabledTools) -> Self {
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
            enabled,
            inner: Arc::new(Mutex::new(state)),
        }
    }

    fn disabled(group: &str) -> String {
        serde_json::json!({
            "error": format!(
                "tool group '{group}' is disabled on this server; reconfigure with --tools"
            )
        })
        .to_string()
    }

    // --- Mail ---------------------------------------------------------------

    #[tool(
        description = "List Mail accounts with identity details (name, email address, type, enabled). Display names alone are often just 'Google'/'Exchange' — use the email field to tell accounts apart. Runs across ALL configured accounts.",
        annotations(read_only_hint = true)
    )]
    async fn mail_list_accounts(&self) -> String {
        if !self.enabled.mail {
            return Self::disabled("mail");
        }
        let mut st = self.inner.lock().await;
        json_result(st.mail.list_accounts().await)
    }

    #[tool(
        description = "List Mail mailboxes per account with message counts (Gmail labels like Work/Personal/Important are separate mailboxes outside the inbox). Call this before mail_search when unsure where mail lives. Optionally narrow to one account by name.",
        annotations(read_only_hint = true)
    )]
    async fn mail_list_mailboxes(&self, Parameters(p): Parameters<MailMailboxesParams>) -> String {
        if !self.enabled.mail {
            return Self::disabled("mail");
        }
        let mut st = self.inner.lock().await;
        json_result(st.mail.list_mailboxes(p.account).await)
    }

    #[tool(
        description = "Search Mail metadata (id, subject, from, date, snippet — never bodies), paginated as {total, offset, limit, results}. By default searches the unified inbox of ALL accounts; pass account to narrow to one account's inbox, or account+mailbox for a specific mailbox (use mail_list_mailboxes to discover them). Use mail_read for full content.",
        annotations(read_only_hint = true)
    )]
    async fn mail_search(&self, Parameters(p): Parameters<MailSearchParams>) -> String {
        if !self.enabled.mail {
            return Self::disabled("mail");
        }
        let mut st = self.inner.lock().await;
        json_result(
            st.mail
                .search(
                    &p.query,
                    p.account.as_deref(),
                    p.mailbox.clone(),
                    p.since.as_deref(),
                    p.limit,
                    p.offset.unwrap_or(0),
                )
                .await,
        )
    }

    #[tool(
        description = "Read one full Mail message by id (an id from mail_search results).",
        annotations(read_only_hint = true)
    )]
    async fn mail_read(&self, Parameters(p): Parameters<MailReadParams>) -> String {
        if !self.enabled.mail {
            return Self::disabled("mail");
        }
        let mut st = self.inner.lock().await;
        json_result(st.mail.read(&p.id).await)
    }

    #[tool(
        description = "Send an email via Mail. Soft-gated: the first call returns status requires_confirmation with a confirmation_token and does NOT send; re-invoke with confirmation_token to execute. Tokens are single-use and expire in 5 minutes.",
        annotations(destructive_hint = true)
    )]
    async fn mail_send(&self, Parameters(p): Parameters<MailSendParams>) -> String {
        if !self.enabled.mail {
            return Self::disabled("mail");
        }
        let mut st = self.inner.lock().await;
        json_result(
            st.mail
                .send(&p.to, &p.subject, &p.body, p.confirmation_token.as_deref())
                .await,
        )
    }

    // --- Messages -----------------------------------------------------------

    #[tool(
        description = "Read recent iMessage/SMS history, newest first. Returns {total, offset, limit, messages:[{from, direction, text, date}]}. chat optionally filters by chat id, display name, or participant handle.",
        annotations(read_only_hint = true)
    )]
    async fn messages_read(&self, Parameters(p): Parameters<MessagesReadParams>) -> String {
        if !self.enabled.messages {
            return Self::disabled("messages");
        }
        let mut st = self.inner.lock().await;
        json_result(
            st.messages
                .read(p.chat, p.limit, p.offset.unwrap_or(0))
                .await,
        )
    }

    #[tool(
        description = "Send an iMessage/SMS. Soft-gated: the first call returns status requires_confirmation with a confirmation_token and does NOT send; re-invoke with confirmation_token to execute. Tokens are single-use and expire in 5 minutes.",
        annotations(destructive_hint = true)
    )]
    async fn messages_send(&self, Parameters(p): Parameters<MessagesSendParams>) -> String {
        if !self.enabled.messages {
            return Self::disabled("messages");
        }
        let mut st = self.inner.lock().await;
        json_result(
            st.messages
                .send(&p.to, &p.body, p.confirmation_token.as_deref())
                .await,
        )
    }

    // --- Notifications ------------------------------------------------------

    #[tool(
        description = "Post a local macOS notification banner (title, optional subtitle, message)."
    )]
    async fn notifications_post(&self, Parameters(p): Parameters<NotificationParams>) -> String {
        if !self.enabled.notifications {
            return Self::disabled("notifications");
        }
        let mut st = self.inner.lock().await;
        json_result(
            st.notifications
                .post(&p.title, &p.message, p.subtitle.as_deref())
                .await,
        )
    }

    // --- Calendar -----------------------------------------------------------

    #[tool(
        description = "List calendar names on this Mac.",
        annotations(read_only_hint = true)
    )]
    async fn calendar_list(&self) -> String {
        if !self.enabled.calendar {
            return Self::disabled("calendar");
        }
        let mut st = self.inner.lock().await;
        json_result(st.calendar.list().await)
    }

    #[tool(
        description = "Read events with start date in [start, end) (ISO 8601). Returns {total, offset, limit, events:[{id, title, start, end, calendar}]}.",
        annotations(read_only_hint = true)
    )]
    async fn calendar_read(&self, Parameters(p): Parameters<CalendarReadParams>) -> String {
        if !self.enabled.calendar {
            return Self::disabled("calendar");
        }
        let mut st = self.inner.lock().await;
        json_result(
            st.calendar
                .read(Some(p.start), Some(p.end), p.limit, p.offset.unwrap_or(0))
                .await,
        )
    }

    #[tool(
        description = "Create a calendar event (first calendar). Soft-gated: re-invoke with confirmation_token to execute. Tokens are single-use, 5-minute TTL.",
        annotations(destructive_hint = true)
    )]
    async fn calendar_create(&self, Parameters(p): Parameters<CalendarCreateParams>) -> String {
        if !self.enabled.calendar {
            return Self::disabled("calendar");
        }
        let mut st = self.inner.lock().await;
        json_result(
            st.calendar
                .create(&p.title, &p.start, &p.end, p.confirmation_token.as_deref())
                .await,
        )
    }

    #[tool(
        description = "Update an event by id (uid from calendar_read); only provided fields change. Soft-gated.",
        annotations(destructive_hint = true)
    )]
    async fn calendar_update(&self, Parameters(p): Parameters<CalendarUpdateParams>) -> String {
        if !self.enabled.calendar {
            return Self::disabled("calendar");
        }
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

    // --- Clipboard ----------------------------------------------------------

    #[tool(
        description = "Read UTF-8 text from the macOS clipboard.",
        annotations(read_only_hint = true)
    )]
    async fn clipboard_get(&self) -> String {
        if !self.enabled.clipboard {
            return Self::disabled("clipboard");
        }
        let st = self.inner.lock().await;
        match st.clipboard.get() {
            Ok(text) => serde_json::json!({ "text": text }).to_string(),
            Err(e) => serde_json::json!({ "error": e.to_string() }).to_string(),
        }
    }

    #[tool(
        description = "Write UTF-8 text to the macOS clipboard.",
        annotations(destructive_hint = true)
    )]
    async fn clipboard_set(&self, Parameters(p): Parameters<ClipboardSetParams>) -> String {
        if !self.enabled.clipboard {
            return Self::disabled("clipboard");
        }
        let st = self.inner.lock().await;
        match st.clipboard.set(&p.text) {
            Ok(()) => serde_json::json!({ "status": "set" }).to_string(),
            Err(e) => serde_json::json!({ "error": e.to_string() }).to_string(),
        }
    }

    // --- Permissions doctor (spec §11.1) ------------------------------------

    #[tool(
        description = "Check macOS Automation permissions for Mail, Calendar and Messages. Run this first if tools return permission errors. Read-only; reports per-app status plus the exact System Settings fix.",
        annotations(read_only_hint = true)
    )]
    async fn permissions_check(&self) -> String {
        let mut probe = personai_core::macos::JxaTransport;
        match permissions::check(&mut probe).await {
            Ok(s) => s,
            Err(e) => serde_json::json!({ "error": e.to_string() }).to_string(),
        }
    }
}

/// The ServerHandler impl. `#[tool_handler]` generates `call_tool` /
/// `get_tool` / `get_info` from the router; `list_tools` is written by hand
#[tool_handler(
    router = Self::tool_router(),
    name = "mcp-macos",
    version = "0.1.0"
)]
impl rmcp::ServerHandler for MacosServer {
    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListToolsResult, rmcp::ErrorData> {
        let supports_cache_hints = context
            .protocol_version()
            .is_some_and(|v| v >= rmcp::model::ProtocolVersion::V_2026_07_28);
        let tools: Vec<rmcp::model::Tool> = Self::tool_router()
            .list_all()
            .into_iter()
            .filter(|t| self.enabled.allows(t.name.as_ref()))
            .collect();
        Ok(rmcp::model::ListToolsResult {
            result_type: Some(rmcp::model::ResultType::COMPLETE),
            tools,
            meta: None,
            next_cursor: None,
            ttl_ms: supports_cache_hints.then_some(0),
            cache_scope: supports_cache_hints.then_some(rmcp::model::CacheScope::Public),
        })
    }
}
#[derive(Deserialize, JsonSchema)]
pub struct MailSearchParams {
    /// Case-insensitive text matched against subject or sender.
    pub query: String,
    /// Optional Mail account name; narrows to that account's inbox.
    #[serde(default)]
    pub account: Option<String>,
    /// Optional mailbox name inside `account` (Gmail labels live here, not
    /// in the inbox). Discover via mail_list_mailboxes.
    #[serde(default)]
    pub mailbox: Option<String>,
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
pub struct MailMailboxesParams {
    /// Optional account name to narrow the listing.
    #[serde(default)]
    pub account: Option<String>,
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

// --- Helpers -------------------------------------------------------------

/// Serializes a toolset result into the single-JSON-string response every
/// tool returns; errors become `{"error": ...}` payloads so the calling
/// agent always receives structured output. Permission failures carry the
/// actionable fix hint from `personai-core`.
fn json_result(r: Result<String, AppleError>) -> String {
    match r {
        Ok(s) => s,
        Err(e) => {
            let mut obj = serde_json::Map::new();
            obj.insert("error".into(), serde_json::json!(e.to_string()));
            if let Some(hint) = e.hint() {
                obj.insert("fix".into(), serde_json::json!(hint));
            }
            serde_json::Value::Object(obj).to_string()
        }
    }
}
