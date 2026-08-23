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
pub mod policy;
pub mod util;

use calendar::CalendarToolset;
use clipboard::ClipboardToolset;
use mail::MailToolset;
use messages::MessagesToolset;
use notifications::NotificationsToolset;
use personai_core::macos::{AppleError, JxaTransport};

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
        on || tool_name == "permissions_check" || tool_name == "mail_config"
    }
}

/// Mutable server state shared by all tools.
///
/// The MCP handler methods take `&self`; everything that must mutate
/// (transports, gates) lives here behind an async mutex, because tool bodies
/// await subprocesses while holding the lock. Each field is one tool group;
/// groups never touch each other.
pub struct ServerState {
    mail: MailToolset<JxaTransport>,
    messages: MessagesToolset<JxaTransport>,
    calendar: CalendarToolset<JxaTransport>,
    notifications: NotificationsToolset<JxaTransport>,
    clipboard: ClipboardToolset,
}

/// The MCP server handle passed to every tool method.
///
/// Carries the effective Mail scope (`scope`) and configured identity/folder
/// policy (`policy`) on top of the mutable toolset state; mail tools resolve
/// and validate against them before delegating.
pub struct MacosServer {
    pub state_dir: std::path::PathBuf,
    enabled: EnabledTools,
    scope: policy::EffectiveScope,
    policy: Arc<policy::MailPolicy>,
    inner: Arc<Mutex<ServerState>>,
}

#[tool_router]
impl MacosServer {
    /// Build a server rooted at `state_dir` with every group enabled, an
    /// open Mail scope and default policy (degraded mode — tests and
    /// unconfigured use); each gated group keeps its own token store under
    /// `state_dir` (`tokens.mail.json`, `tokens.messages.json`, …).
    pub fn new(state_dir: std::path::PathBuf) -> Self {
        Self::new_with_tools(
            state_dir,
            EnabledTools::all(),
            policy::MailPolicy::default(),
            policy::EffectiveScope::open(),
        )
    }

    /// Build a server exposing only the enabled groups (spec §11.1), with
    /// the Mail identity/folder policy and effective scope resolved by the
    /// caller (`main.rs`: CLI/file policy validated against live folders).
    pub fn new_with_tools(
        state_dir: std::path::PathBuf,
        enabled: EnabledTools,
        policy: policy::MailPolicy,
        scope: policy::EffectiveScope,
    ) -> Self {
        let gated = |store: &str| state_dir.join(store);
        let state = ServerState {
            mail: MailToolset::with_gate(JxaTransport, gated("tokens.mail.json")).unwrap_or_else(
                |e| {
                    eprintln!("mail gate unavailable ({e}); mail_send will refuse");
                    MailToolset::new(JxaTransport)
                },
            ),
            messages: MessagesToolset::with_gate(JxaTransport, gated("tokens.messages.json"))
                .unwrap_or_else(|e| {
                    eprintln!("messages gate unavailable ({e}); messages_send will refuse");
                    MessagesToolset::new(JxaTransport)
                }),
            calendar: CalendarToolset::with_gate(JxaTransport, gated("tokens.calendar.json"))
                .unwrap_or_else(|e| {
                    eprintln!("calendar gate unavailable ({e}); writes will refuse");
                    CalendarToolset::new(JxaTransport)
                }),
            notifications: NotificationsToolset::new(JxaTransport),
            clipboard: ClipboardToolset::new(),
        };
        Self {
            state_dir,
            enabled,
            policy: Arc::new(policy),
            scope,
            inner: Arc::new(Mutex::new(state)),
        }
    }

    /// Actionable out-of-scope error for a folder selection, carrying the
    /// effective allowlist (`EffectiveScope::summary`, capped at 20 entries).
    fn out_of_scope(folder: &str, scope: &policy::EffectiveScope) -> AppleError {
        AppleError::Transport(format!(
            "folder '{folder}' outside configured scope; valid: {:?}",
            scope.summary()
        ))
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
        description = "Search Mail metadata (id, subject, from, date, snippet, folder — never bodies), paginated as {total, offset, limit, results, scanned_per_folder, truncated}. Pass folders=[\"Account/Mailbox\", …] to select one or more specific mailboxes for this call (each entry must lie inside the configured scope — run mail_config to see it; split on the last '/'); otherwise account narrows to that account's inbox (account+mailbox targets a specific mailbox), and with neither the unified inbox of ALL accounts is searched. truncated:true means the scan budget expired mid-search: results are partial, with per-folder counts in scanned_per_folder. Use mail_read for full content. Params: since=ISO 8601 (YYYY-MM-DD or YYYY-MM-DDTHH:MM:SSZ); limit/offset page the merged results (default 20, max 100). Example: mail_search(query=\"interview\", folders=[\"iCloud/Inbox\"]).",
        annotations(read_only_hint = true)
    )]
    async fn mail_search(&self, Parameters(p): Parameters<MailSearchParams>) -> String {
        if !self.enabled.mail {
            return Self::disabled("mail");
        }
        // Legacy sugar kept intact: a bare `account` still means that
        // account's inbox via the single-target engine.
        if p.folders.as_ref().is_none_or(Vec::is_empty)
            && p.mailbox.is_none()
            && p.account.is_some()
        {
            let mut st = self.inner.lock().await;
            return json_result(
                st.mail
                    .search(
                        &p.query,
                        p.account.as_deref(),
                        None,
                        p.since.as_deref(),
                        p.limit,
                        p.offset.unwrap_or(0),
                    )
                    .await,
            );
        }
        // Per-call selection within the effective scope. Each entry is
        // "Account/Mailbox", split on the LAST '/'; violations are rejected
        // before any transport work, with the ≤20-entry valid list attached.
        let mut pairs: Vec<(String, String)> = Vec::new();
        if let Some(folders) = &p.folders {
            for f in folders.iter().filter(|f| !f.is_empty()) {
                match f.rsplit_once('/') {
                    Some((account, mailbox)) if self.scope.allows(account, mailbox) => {
                        if !pairs.iter().any(|(a, m)| a == account && m == mailbox) {
                            pairs.push((account.to_string(), mailbox.to_string()));
                        }
                    }
                    _ => return json_result(Err(Self::out_of_scope(f, &self.scope))),
                }
            }
        }
        let targets = if pairs.is_empty() {
            // No `folders` (or all blank): fall back to the legacy sugar,
            // validated against the same scope.
            match (p.account.as_deref(), p.mailbox.as_deref()) {
                (Some(account), Some(mailbox)) => {
                    if self.scope.allows(account, mailbox) {
                        mail::MailTargets::Folders(vec![(account.to_string(), mailbox.to_string())])
                    } else {
                        return json_result(Err(Self::out_of_scope(
                            &format!("{account}/{mailbox}"),
                            &self.scope,
                        )));
                    }
                }
                _ => mail::MailTargets::Unified,
            }
        } else {
            mail::MailTargets::Folders(pairs)
        };
        let limit = p.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
        let mut st = self.inner.lock().await;
        json_result(
            st.mail
                .search_multi(
                    &targets,
                    &p.query,
                    p.since.as_deref(),
                    limit,
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
        description = "Send an email via Mail, optionally from a specific identity: from takes a live account name or its primary email (see mail_list_accounts); when omitted, the configured default_from applies, else Mail's own default. Invalid identities are rejected with a valid_from list. Soft-gated: the first call returns status requires_confirmation with a confirmation_token and does NOT send; re-invoke with confirmation_token to execute. Tokens are single-use and expire in 5 minutes. Example: mail_send(to=\"peer@example.com\", subject=\"Lunch\", body=\"Noon ok?\").",
        annotations(destructive_hint = true)
    )]
    async fn mail_send(&self, Parameters(p): Parameters<MailSendParams>) -> String {
        if !self.enabled.mail {
            return Self::disabled("mail");
        }
        let mut st = self.inner.lock().await;
        // Identity policy: an explicit `from` must match a live account by
        // name or primary email (case-insensitive) before anything sends.
        let from = match p.from.as_deref() {
            None => None,
            Some(want) => {
                let raw = match st.mail.list_accounts().await {
                    Ok(s) => s,
                    Err(e) => return json_result(Err(e)),
                };
                let parsed: serde_json::Value = match serde_json::from_str(&raw) {
                    Ok(v) => v,
                    Err(e) => {
                        return json_result(Err(AppleError::Transport(format!(
                            "unreadable Mail account listing: {e}"
                        ))));
                    }
                };
                let accounts = parsed
                    .get("accounts")
                    .and_then(serde_json::Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                fn name_of(a: &serde_json::Value) -> &str {
                    a.get("name")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                }
                fn email_of(a: &serde_json::Value) -> &str {
                    a.get("email")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                }
                let hit = accounts.iter().any(|a| {
                    name_of(a).eq_ignore_ascii_case(want)
                        || (!email_of(a).is_empty() && email_of(a).eq_ignore_ascii_case(want))
                });
                if !hit {
                    let valid_from: Vec<String> = accounts
                        .iter()
                        .take(20)
                        .map(|a| match email_of(a) {
                            "" => name_of(a).to_string(),
                            email => format!("{} <{email}>", name_of(a)),
                        })
                        .collect();
                    return json_result(Err(AppleError::Transport(format!(
                        "from '{want}' is not a configured Mail account; valid_from: {valid_from:?}"
                    ))));
                }
                Some(want.to_string())
            }
        };
        json_result(
            st.mail
                .send(
                    &p.to,
                    &p.subject,
                    &p.body,
                    from.as_deref(),
                    p.confirmation_token.as_deref(),
                )
                .await,
        )
    }

    #[tool(
        description = "Forward an existing Mail message to a recipient using Mail's native forward verb — sent from the account owning the source message, threading headers preserved. comment adds optional text above the forwarded content. Soft-gated: the first call returns status requires_confirmation with a confirmation_token and does NOT forward; re-invoke with confirmation_token to execute. Tokens are single-use and expire in 5 minutes. Example: mail_forward(id=\"A1B2\", to=\"team@example.com\").",
        annotations(destructive_hint = true)
    )]
    async fn mail_forward(&self, Parameters(p): Parameters<MailForwardParams>) -> String {
        if !self.enabled.mail {
            return Self::disabled("mail");
        }
        let mut st = self.inner.lock().await;
        json_result(
            st.mail
                .forward(
                    &p.id,
                    &p.to,
                    p.comment.as_deref(),
                    p.confirmation_token.as_deref(),
                )
                .await,
        )
    }

    #[tool(
        description = "Reply to an existing Mail message using Mail's native reply verb — sent from the account owning the source message, threading headers preserved; body supplies the reply text (recipients come from the original). Soft-gated: the first call returns status requires_confirmation with a confirmation_token and does NOT send; re-invoke with confirmation_token to execute. Tokens are single-use and expire in 5 minutes. Example: mail_reply(id=\"A1B2\", body=\"Confirmed, thanks!\").",
        annotations(destructive_hint = true)
    )]
    async fn mail_reply(&self, Parameters(p): Parameters<MailReplyParams>) -> String {
        if !self.enabled.mail {
            return Self::disabled("mail");
        }
        let mut st = self.inner.lock().await;
        json_result(
            st.mail
                .reply(&p.id, &p.body, p.confirmation_token.as_deref())
                .await,
        )
    }

    #[tool(
        description = "Read-only doctor for the active Mail scope policy: returns {mode, folders, default_from, deny_set}, where mode is \"open\", \"explicit\" or \"default-deny-set\", folders lists the effective allowlist of Account/Mailbox entries accepted by mail_search, default_from is the fallback send identity and deny_set the mailbox names excluded when no explicit allowlist is configured. No arguments, no side effects.",
        annotations(read_only_hint = true)
    )]
    async fn mail_config(&self) -> String {
        serde_json::json!({
            "mode": self.scope.mode(),
            "folders": self.scope.summary(),
            "default_from": self.policy.default_from.clone(),
            "deny_set": policy::DEFAULT_DENY,
        })
        .to_string()
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
        description = "Read events with start date in [start, end) (ISO 8601). Returns {total, offset, limit, events:[{id, title, start, end, calendar}]}. start inclusive, end exclusive; both ISO 8601. Example: calendar_read(start=\"2026-08-23\", end=\"2026-08-24\").",
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
        description = "Create a calendar event (first calendar). Soft-gated: first call returns requires_confirmation + confirmation_token and does NOT create; re-invoke with the token. Tokens single-use, 5-minute TTL. Example: calendar_create(title=\"Interview\", start=\"2026-08-25T09:00:00Z\", end=\"2026-08-25T10:00:00Z\").",
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
        description = "Update an event by id (uid from calendar_read); only provided fields change. Soft-gated: first call returns requires_confirmation + confirmation_token and does NOT modify; re-invoke with the token (single-use, 5-min TTL). Example: calendar_update(id=\"abc\", start=\"2026-08-25T10:00:00Z\").",
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
    version = "0.1.3",
    instructions = "Purpose-built Mail/Messages/Calendar/Notifications/Clipboard tools; prefer them over raw osascript/AppleScript. List/search results are summary-first metadata + snippet, never bodies — fetch full content by id with mail_read/messages_read. Paginated `offset`+`limit` (default 20, max 100). Sends and calendar writes are soft-gated: first call returns `requires_confirmation` + a single-use confirmation_token (5-min TTL); re-invoke with the token to execute. Reads, notifications, and clipboard are ungated. On permission errors, run `permissions_check` first. For status/history, consult the personai state directory before querying these apps. Mail access is selection-within-an-allowlist: mail_config reports the effective scope, and mail_search takes folders=[\"Account/Mailbox\", …] to pick any targets inside it per call. mail_forward/mail_reply send from the account owning the source message; a truncated:true search result means the scan hit its time budget and returned partial results (per-folder counts in scanned_per_folder)."
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
    /// Optional per-call folder selection: `["Account/Mailbox", …]` (split
    /// on the last '/'). Every entry must lie inside the configured scope —
    /// run mail_config for the current allowlist. Takes precedence over
    /// account/mailbox.
    #[serde(default)]
    pub folders: Option<Vec<String>>,
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
    /// Optional sender identity: a live account name or its primary email
    /// (see mail_list_accounts). Omit for the configured default_from, else
    /// Mail's own default. Invalid values are rejected with a valid_from list.
    #[serde(default)]
    pub from: Option<String>,
    /// Token from a previous requires_confirmation response.
    #[serde(default)]
    pub confirmation_token: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct MailForwardParams {
    /// Message id from a mail_search result.
    pub id: String,
    /// Recipient address for the forwarded message.
    pub to: String,
    /// Optional comment placed above the forwarded content.
    #[serde(default)]
    pub comment: Option<String>,
    /// Token from a previous requires_confirmation response.
    #[serde(default)]
    pub confirmation_token: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct MailReplyParams {
    /// Message id from a mail_search result.
    pub id: String,
    /// Plain-text reply body (recipients come from the original message).
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
