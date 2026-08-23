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
pub mod contacts;
pub mod mail;
pub mod messages;
pub mod notifications;
pub mod permissions;
pub mod policy;
pub mod reminders;
pub mod util;

use calendar::CalendarToolset;
use clipboard::ClipboardToolset;
use contacts::ContactsToolset;
use mail::MailToolset;
use messages::MessagesToolset;
use notifications::NotificationsToolset;
use personai_core::macos::{AppleError, JxaTransport};
use reminders::RemindersToolset;

/// Default page size for list/search tools.
pub(crate) const DEFAULT_LIMIT: u32 = 20;
/// Hard maximum page size (context discipline: no unbounded blobs).
pub(crate) const MAX_LIMIT: u32 = 100;

/// Default per-mailbox scan depth for searches (`scan_limit` omitted).
/// Bulk Apple Events fetch whole mailboxes regardless, so depth only
/// bounds post-processing — cheap to keep generous.
pub(crate) const DEFAULT_SCAN: u32 = 5000;
/// Ceiling for `scan_limit` (outlier mailboxes; budget checks still apply).
pub(crate) const MAX_SCAN: u32 = 25_000;
/// Maximum OR terms per search (query + any_of combined).
pub(crate) const MAX_TERMS: usize = 8;

/// Consumer-side tool-set trimming (spec §11.1): a client can load only the
/// groups it needs via `--tools mail,calendar`. `permissions_check` is
/// always available.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EnabledTools {
    pub mail: bool,
    pub messages: bool,
    pub calendar: bool,
    pub contacts: bool,
    pub reminders: bool,
    pub notifications: bool,
    pub clipboard: bool,
}

impl EnabledTools {
    pub fn all() -> Self {
        Self {
            mail: true,
            messages: true,
            calendar: true,
            contacts: true,
            reminders: true,
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
            contacts: false,
            reminders: false,
            notifications: false,
            clipboard: false,
        };
        for name in csv.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match name {
                "mail" => e.mail = true,
                "messages" => e.messages = true,
                "calendar" => e.calendar = true,
                "contacts" => e.contacts = true,
                "reminders" => e.reminders = true,
                "notifications" => e.notifications = true,
                "clipboard" => e.clipboard = true,
                other => {
                    return Err(format!(
                        "unknown tool group '{other}' (valid: mail, messages, calendar, contacts, reminders, notifications, clipboard)"
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
        } else if tool_name.starts_with("contacts_") {
            self.contacts
        } else if tool_name.starts_with("reminders_") {
            self.reminders
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
/// The MCP handler methods take `&self`; each group's toolset must mutate
/// (transports, gates), so every field carries its own async mutex — tool
/// bodies await subprocesses while holding it. One mutex PER GROUP: a slow
/// mail_search must not block a concurrent clipboard_get, and agents do
/// issue parallel calls. Groups never touch each other.
pub struct ServerState {
    mail: Mutex<MailToolset<JxaTransport>>,
    messages: Mutex<MessagesToolset<JxaTransport>>,
    calendar: Mutex<CalendarToolset<JxaTransport>>,
    contacts: Mutex<ContactsToolset<JxaTransport>>,
    reminders: Mutex<RemindersToolset<JxaTransport>>,
    notifications: Mutex<NotificationsToolset<JxaTransport>>,
    clipboard: Mutex<ClipboardToolset>,
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
    inner: Arc<ServerState>,
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
            mail: Mutex::new(
                MailToolset::with_gate(JxaTransport, gated("tokens.mail.json")).unwrap_or_else(
                    |e| {
                        eprintln!("mail gate unavailable ({e}); mail_send will refuse");
                        MailToolset::new(JxaTransport)
                    },
                ),
            ),
            messages: Mutex::new(
                MessagesToolset::with_gate(JxaTransport, gated("tokens.messages.json"))
                    .unwrap_or_else(|e| {
                        eprintln!("messages gate unavailable ({e}); messages_send will refuse");
                        MessagesToolset::new(JxaTransport)
                    }),
            ),
            calendar: Mutex::new(
                CalendarToolset::with_gate(JxaTransport, gated("tokens.calendar.json"))
                    .unwrap_or_else(|e| {
                        eprintln!("calendar gate unavailable ({e}); writes will refuse");
                        CalendarToolset::new(JxaTransport)
                    }),
            ),
            contacts: Mutex::new(ContactsToolset::new(JxaTransport)),
            reminders: Mutex::new(
                RemindersToolset::with_gate(JxaTransport, gated("tokens.reminders.json"))
                    .unwrap_or_else(|e| {
                        eprintln!("reminders gate unavailable ({e}); writes will refuse");
                        RemindersToolset::new(JxaTransport)
                    }),
            ),
            notifications: Mutex::new(NotificationsToolset::new(JxaTransport)),
            clipboard: Mutex::new(ClipboardToolset::new()),
        };
        Self {
            state_dir,
            enabled,
            policy: Arc::new(policy),
            scope,
            inner: Arc::new(state),
        }
    }

    /// Scope-rejection error carrying an HONEST picture of the universe
    /// (mode, full folder list, count). A truncated list here once led an
    /// agent to conclude most folders were out of scope.
    fn out_of_scope(folder: &str, scope: &policy::EffectiveScope) -> AppleError {
        AppleError::Transport(format!(
            "folder '{folder}' outside configured scope; {}",
            policy::format_scope_hint(scope)
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
        description = "List Mail accounts with identity details (name, email address, type, enabled) across ALL configured accounts. Run this at the START of any multi-step Mail task so account-scoped calls use exact names — display names are often just 'Google'/'Exchange'; the email field disambiguates.",
        annotations(read_only_hint = true)
    )]
    async fn mail_list_accounts(&self) -> String {
        if !self.enabled.mail {
            return Self::disabled("mail");
        }
        let mut st = self.inner.mail.lock().await;
        json_result(st.list_accounts().await)
    }

    #[tool(
        description = "List Mail mailboxes per account with message counts (Gmail labels like Work/Personal/Important are separate mailboxes outside the inbox). Call this before mail_search when unsure where mail lives. Optionally narrow to one account by name.",
        annotations(read_only_hint = true)
    )]
    async fn mail_list_mailboxes(&self, Parameters(p): Parameters<MailMailboxesParams>) -> String {
        if !self.enabled.mail {
            return Self::disabled("mail");
        }
        let mut st = self.inner.mail.lock().await;
        json_result(st.list_mailboxes(p.account).await)
    }

    #[tool(
        description = "Search the user's email — THE tool for every find/check/read/summarize/triage-email request ('mail about X', 'status of my job applications', 'find receipts'), replacing AppleScript/osascript shell scripts that are slow on real mailboxes, unbounded in output, and unscoped. Terms (query + any_of, OR-combined, ≤8) match case-insensitively against subject or sender; NO terms = match-all census — pair with group_by=\"sender\" to collapse hundreds of messages into one page of {key, count, first, last, latest_id, sample_subjects, folders}. Metadata only ({id, subject, from, date, folder}; snippets off by default — mail_read individual rows), paginated as {total, offset, limit, results|groups, scanned_per_folder, truncated}. Target folders via folders=[\"Account/Mailbox\", …] (inside the configured scope — run mail_config; \"*\" sweeps every scoped folder; a NAMED deny-set folder like [Gmail]/Spam is admitted with scope_note); else account (+mailbox) narrows to one box; else the unified inbox. scan_limit raises per-mailbox depth past the 5000 default when message counts demand it.",
        annotations(read_only_hint = true)
    )]
    async fn mail_search(&self, Parameters(p): Parameters<MailSearchParams>) -> String {
        if !self.enabled.mail {
            return Self::disabled("mail");
        }
        if p.group_by.is_some()
            && !matches!(p.group_by.as_deref(), Some("sender") | Some("subject"))
        {
            return serde_json::json!({"error": "group_by must be \"sender\" or \"subject\""})
                .to_string();
        }
        let snippets = p.include_snippets.unwrap_or(false);
        let scan = p.scan_limit.map_or(DEFAULT_SCAN, |s| s.clamp(1, MAX_SCAN));
        // OR term set: query ∪ any_of; empty set = match-all census.
        let mut terms: Vec<String> = Vec::new();
        if let Some(q) = p.query.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
            terms.push(q.to_string());
        }
        if let Some(any) = &p.any_of {
            terms.extend(
                any.iter()
                    .filter(|t| !t.trim().is_empty())
                    .map(|t| t.trim().to_string()),
            );
        }
        if terms.len() > MAX_TERMS {
            return serde_json::json!({
                "error": format!(
                    "too many search terms ({} > {MAX_TERMS}); merge or drop some",
                    terms.len()
                )
            })
            .to_string();
        }
        let group = p
            .group_by
            .as_deref()
            .filter(|s| !s.is_empty())
            .and_then(mail::MailGroupBy::parse);
        if p.folders.as_ref().is_none_or(Vec::is_empty)
            && p.mailbox.is_none()
            && p.account.is_some()
            && group.is_none()
        {
            let mut st = self.inner.mail.lock().await;
            return json_result(
                st.search(
                    &p.query.unwrap_or_default(),
                    &terms,
                    p.account.as_deref(),
                    None,
                    p.since.as_deref(),
                    p.limit,
                    p.offset.unwrap_or(0),
                    scan,
                    snippets,
                )
                .await,
            );
        }
        // Per-call selection within the effective scope. Each entry is
        // "Account/Mailbox", split on the LAST '/'; violations are rejected
        // before any transport work. `"*"` expands to every scoped folder.
        let (pairs, denied_admitted) = if p
            .folders
            .as_ref()
            .is_some_and(|fs| fs.iter().any(|f| f == "*"))
        {
            if self.scope.is_open() {
                return serde_json::json!({"error": "folders:[\"*\"] unavailable in open scope (no startup snapshot); name folders explicitly"})
                    .to_string();
            }
            (self.scope.all(), false)
        } else {
            let entries: Vec<String> = p
                .folders
                .clone()
                .unwrap_or_default()
                .into_iter()
                .filter(|f| !f.is_empty())
                .collect();
            let (pairs, rejected, denied) = policy::resolve_selection(&entries, &self.scope);
            if let Some(first) = rejected.first() {
                return json_result(Err(Self::out_of_scope(first, &self.scope)));
            }
            (pairs, denied)
        };
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
        let mut st = self.inner.mail.lock().await;
        let result = st
            .search_multi(
                &targets,
                terms.first().map(String::as_str).unwrap_or(""),
                &terms.iter().skip(1).cloned().collect::<Vec<_>>(),
                p.since.as_deref(),
                limit,
                p.offset.unwrap_or(0),
                group,
                scan,
                snippets,
            )
            .await;
        match result {
            Ok(s) if denied_admitted => {
                // Named ask reached a deny-set folder — flag it so callers
                // know the sweep semantics differed from the default.
                match serde_json::from_str::<serde_json::Value>(&s) {
                    Ok(mut v) => {
                        if let Some(o) = v.as_object_mut() {
                            o.insert(
                                "scope_note".into(),
                                serde_json::json!("denied-folder-explicit"),
                            );
                        }
                        v.to_string()
                    }
                    Err(_) => s,
                }
            }
            other => json_result(other),
        }
    }

    #[tool(
        description = "Read ONE full Mail message by id (ids come from mail_search results). Use when a search snippet is not enough — e.g. checking body wording for an offer, rejection, or interview detail before reporting application status. Pass folder=\"Account/Mailbox\" from the search row to target its mailbox directly; without it the inbox is tried first, then all mailboxes. Bodies are capped at ~20k characters (body_truncated:true when clipped).",
        annotations(read_only_hint = true)
    )]
    async fn mail_read(&self, Parameters(p): Parameters<MailReadParams>) -> String {
        if !self.enabled.mail {
            return Self::disabled("mail");
        }
        if let Some(f) = p.folder.as_deref() {
            let admitted = match f.rsplit_once('/') {
                Some((account, mailbox)) => {
                    self.scope.allows(account, mailbox)
                        || (!self.scope.is_explicit()
                            && !self.scope.is_open()
                            && policy::deny_matches(mailbox.trim()))
                }
                None => false,
            };
            if admitted {
                let mut st = self.inner.mail.lock().await;
                return json_result(st.read(&p.id, Some(f)).await);
            }
            return json_result(Err(Self::out_of_scope(f, &self.scope)));
        }
        let mut st = self.inner.mail.lock().await;
        json_result(st.read(&p.id, None).await)
    }

    #[tool(
        description = "Send an email via Mail, optionally from a specific identity: from takes a live account name or its primary email (see mail_list_accounts); when omitted, the configured default_from applies, else Mail's own default. Invalid identities are rejected with a valid_from list. Soft-gated: the first call returns status requires_confirmation with a confirmation_token and does NOT send; re-invoke with confirmation_token to execute. Tokens are single-use and expire in 5 minutes. Example: mail_send(to=\"peer@example.com\", subject=\"Lunch\", body=\"Noon ok?\").",
        annotations(destructive_hint = true)
    )]
    async fn mail_send(&self, Parameters(p): Parameters<MailSendParams>) -> String {
        if !self.enabled.mail {
            return Self::disabled("mail");
        }
        let mut st = self.inner.mail.lock().await;
        // Identity policy: an explicit `from` must match a live account by
        // name or primary email (case-insensitive) before anything sends.
        let from = match p.from.as_deref() {
            None => None,
            Some(want) => {
                let raw = match st.list_accounts().await {
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
            st.send(
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
        let mut st = self.inner.mail.lock().await;
        json_result(
            st.forward(
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
        let mut st = self.inner.mail.lock().await;
        json_result(
            st.reply(&p.id, &p.body, p.confirmation_token.as_deref())
                .await,
        )
    }

    #[tool(
        description = "Read-only doctor for the active Mail scope policy: returns {mode, folders (FULL effective list of Account/Mailbox entries accepted by mail_search — not truncated), default_from, deny_set (mailbox names excluded when no explicit allowlist is configured), state_dir (personai state directory; consult job-apps.json there for status/history tasks), state_files (json files currently in state_dir)}. No arguments, no side effects.",
        annotations(read_only_hint = true)
    )]
    async fn mail_config(&self) -> String {
        let state_files = std::fs::read_dir(&self.state_dir)
            .map(|rd| {
                let mut names: Vec<String> = rd
                    .flatten()
                    .filter_map(|e| e.file_name().into_string().ok())
                    .filter(|n| n.ends_with(".json") || n.ends_with(".jsonl"))
                    .collect();
                names.sort();
                names
            })
            .unwrap_or_default();
        serde_json::json!({
            "mode": self.scope.mode(),
            "folders": self.scope.all(),
            "default_from": self.policy.default_from.clone(),
            "deny_set": policy::DEFAULT_DENY,
            "state_dir": self.state_dir.display().to_string(),
            "state_files": state_files,
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
        let mut st = self.inner.messages.lock().await;
        json_result(st.read(p.chat, p.limit, p.offset.unwrap_or(0)).await)
    }

    #[tool(
        description = "List iMessage/SMS chats: identifier, display name, service, a sample participant handle, message count, and last activity — most recent first. THE way to resolve 'send to NAME' into a concrete handle before messages_send.",
        annotations(read_only_hint = true)
    )]
    async fn messages_chats(&self, Parameters(p): Parameters<MessagesReadParams>) -> String {
        if !self.enabled.messages {
            return Self::disabled("messages");
        }
        let mut st = self.inner.messages.lock().await;
        json_result(st.chats(p.limit, p.offset.unwrap_or(0)).await)
    }

    #[tool(
        description = "Send an iMessage/SMS. Soft-gated: the first call returns status requires_confirmation with a confirmation_token and does NOT send; re-invoke with confirmation_token to execute. Tokens are single-use and expire in 5 minutes.",
        annotations(destructive_hint = true)
    )]
    async fn messages_send(&self, Parameters(p): Parameters<MessagesSendParams>) -> String {
        if !self.enabled.messages {
            return Self::disabled("messages");
        }
        let mut st = self.inner.messages.lock().await;
        json_result(
            st.send(&p.to, &p.body, p.confirmation_token.as_deref())
                .await,
        )
    }

    // --- Contacts -----------------------------------------------------------

    #[tool(
        description = "Search macOS Contacts (READ-ONLY): case-insensitive substring over name, organization, and emails; empty query lists the directory. Returns {total, offset, limit, contacts:[{id, name, organization, emails, phones}]} paginated. Use to resolve 'email Mom' / 'text Sam' into concrete addresses and handles.",
        annotations(read_only_hint = true)
    )]
    async fn contacts_search(&self, Parameters(p): Parameters<ContactsSearchParams>) -> String {
        if !self.enabled.contacts {
            return Self::disabled("contacts");
        }
        let mut st = self.inner.contacts.lock().await;
        json_result(
            st.search(
                p.query.as_deref().unwrap_or(""),
                p.limit,
                p.offset.unwrap_or(0),
            )
            .await,
        )
    }

    // --- Reminders ----------------------------------------------------------

    #[tool(
        description = "List reminder list names on this Mac.",
        annotations(read_only_hint = true)
    )]
    async fn reminders_list_lists(&self) -> String {
        if !self.enabled.reminders {
            return Self::disabled("reminders");
        }
        let mut st = self.inner.reminders.lock().await;
        json_result(st.list_lists().await)
    }

    #[tool(
        description = "Read reminders (open items unless include_completed). Optional list name filter. Returns {total, offset, limit, reminders:[{id, name, due, body, priority, completed}]}.",
        annotations(read_only_hint = true)
    )]
    async fn reminders_read(&self, Parameters(p): Parameters<RemindersReadParams>) -> String {
        if !self.enabled.reminders {
            return Self::disabled("reminders");
        }
        let mut st = self.inner.reminders.lock().await;
        json_result(
            st.read(
                p.list.clone(),
                p.include_completed.unwrap_or(false),
                p.limit,
                p.offset.unwrap_or(0),
            )
            .await,
        )
    }

    #[tool(
        description = "Create a reminder (optional list, due date ISO 8601, notes). Soft-gated: first call returns requires_confirmation + confirmation_token and does NOT create; re-invoke with the token (single-use, 5-min TTL).",
        annotations(destructive_hint = true)
    )]
    async fn reminders_create(&self, Parameters(p): Parameters<RemindersCreateParams>) -> String {
        if !self.enabled.reminders {
            return Self::disabled("reminders");
        }
        let mut st = self.inner.reminders.lock().await;
        json_result(
            st.create(
                &p.name,
                p.list.as_deref(),
                p.due.as_deref(),
                p.notes.as_deref(),
                p.confirmation_token.as_deref(),
            )
            .await,
        )
    }

    #[tool(
        description = "Mark a reminder completed by id (from reminders_read). Soft-gated like reminders_create.",
        annotations(destructive_hint = true)
    )]
    async fn reminders_complete(
        &self,
        Parameters(p): Parameters<RemindersCompleteParams>,
    ) -> String {
        if !self.enabled.reminders {
            return Self::disabled("reminders");
        }
        let mut st = self.inner.reminders.lock().await;
        json_result(st.complete(&p.id, p.confirmation_token.as_deref()).await)
    }
    // --- Notifications ------------------------------------------------------

    #[tool(
        description = "Post a local macOS notification banner (title, optional subtitle, message)."
    )]
    async fn notifications_post(&self, Parameters(p): Parameters<NotificationParams>) -> String {
        if !self.enabled.notifications {
            return Self::disabled("notifications");
        }
        let mut st = self.inner.notifications.lock().await;
        json_result(st.post(&p.title, &p.message, p.subtitle.as_deref()).await)
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
        let mut st = self.inner.calendar.lock().await;
        json_result(st.list().await)
    }

    #[tool(
        description = "Read events with start date in [start, end) (ISO 8601). Returns {total, offset, limit, events:[{id, title, start, end, calendar}]}. start inclusive, end exclusive; both ISO 8601. Example: calendar_read(start=\"2026-08-23\", end=\"2026-08-24\").",
        annotations(read_only_hint = true)
    )]
    async fn calendar_read(&self, Parameters(p): Parameters<CalendarReadParams>) -> String {
        if !self.enabled.calendar {
            return Self::disabled("calendar");
        }
        let mut st = self.inner.calendar.lock().await;
        json_result(
            st.read(Some(p.start), Some(p.end), p.limit, p.offset.unwrap_or(0))
                .await,
        )
    }

    #[tool(
        description = "Create a calendar event on a named calendar (default: first calendar, reported back). Optional location and notes. Soft-gated: first call returns requires_confirmation + confirmation_token and does NOT create; re-invoke with the token. Tokens single-use, 5-minute TTL. Example: calendar_create(title=\"Interview\", start=\"2026-08-25T09:00:00Z\", end=\"2026-08-25T10:00:00Z\", calendar=\"Work\").",
        annotations(destructive_hint = true)
    )]
    async fn calendar_create(&self, Parameters(p): Parameters<CalendarCreateParams>) -> String {
        if !self.enabled.calendar {
            return Self::disabled("calendar");
        }
        let mut st = self.inner.calendar.lock().await;
        json_result(
            st.create(
                &p.title,
                &p.start,
                &p.end,
                p.calendar.as_deref(),
                p.location.as_deref(),
                p.notes.as_deref(),
                p.confirmation_token.as_deref(),
            )
            .await,
        )
    }
    #[tool(
        description = "Update an event by id (uid from calendar_read); only provided fields change (title, start, end, location, notes). Soft-gated: first call returns requires_confirmation + confirmation_token and does NOT modify; re-invoke with the token (single-use, 5-min TTL). Example: calendar_update(id=\"abc\", start=\"2026-08-25T10:00:00Z\").",
        annotations(destructive_hint = true)
    )]
    async fn calendar_update(&self, Parameters(p): Parameters<CalendarUpdateParams>) -> String {
        if !self.enabled.calendar {
            return Self::disabled("calendar");
        }
        let mut st = self.inner.calendar.lock().await;
        json_result(
            st.update(
                &p.id,
                p.title.as_deref(),
                p.start.as_deref(),
                p.end.as_deref(),
                p.location.as_deref(),
                p.notes.as_deref(),
                p.confirmation_token.as_deref(),
            )
            .await,
        )
    }

    #[tool(
        description = "Delete an event by id (uid from calendar_read). Marks the event deleted — recoverable in Calendar's UI. Soft-gated like other writes.",
        annotations(destructive_hint = true)
    )]
    async fn calendar_delete(&self, Parameters(p): Parameters<CalendarDeleteParams>) -> String {
        if !self.enabled.calendar {
            return Self::disabled("calendar");
        }
        let mut st = self.inner.calendar.lock().await;
        json_result(st.delete(&p.id, p.confirmation_token.as_deref()).await)
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
        let st = self.inner.clipboard.lock().await;
        match st.get() {
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
        let st = self.inner.clipboard.lock().await;
        match st.set(&p.text) {
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
/// below.
#[tool_handler(
    router = Self::tool_router(),
    name = "mcp-macos",
    version = "0.1.7",
    instructions = "macOS automation suite: Mail, Messages (iMessage/SMS), Calendar, Notifications, Clipboard. ROUTING — for anything touching those apps ALWAYS use these tools instead of osascript/AppleScript/JXA via shell; raw scripting is slow on real mailboxes, returns unbounded output, and bypasses scoping plus safety gates. Triggers: check/find/read/summarize/triage email or job-application status -> mail_search then mail_read; send/forward/reply email -> mail_send/mail_forward/mail_reply; iMessage/SMS history -> messages_read, sends -> messages_send; calendar events -> calendar_list/calendar_read/calendar_create/calendar_update/calendar_delete; recipient names (people) -> contacts_search first, chat handles -> messages_chats first; task items -> reminders_read/reminders_create/reminders_complete; clipboard text -> clipboard_get/clipboard_set; any permission error -> permissions_check. Typical flow: mail_list_accounts -> mail_list_mailboxes -> mail_search(query, since=..., folders=[...]) -> mail_read(id) only where a snippet is not enough. Results are summary-first metadata + snippet, never bodies; pages default 20 / max 100 - iterate offset instead of dumping output. Sends and calendar writes are soft-gated: first call returns requires_confirmation + single-use confirmation_token (5-min TTL); re-invoke with the token to execute; reads, notifications, clipboard are ungated. Error payloads carry actionable fix hints - follow them. For status/history tasks consult the personai state directory before querying apps and record findings there after."
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
    /// Case-insensitive text matched against subject or sender. Omitted or
    /// blank = match-all — pair with group_by="sender" for a one-call
    /// census of every sender in the target folders.
    #[serde(default)]
    pub query: Option<String>,
    /// Extra OR terms (query + any_of together, max 8): a row matches when
    /// ANY term hits subject or sender. One call replaces keyword fan-out.
    #[serde(default)]
    pub any_of: Option<Vec<String>>,
    /// Per-mailbox scan depth, newest first (default 5000, max 25000). The
    /// bulk metadata fetch already reads the whole mailbox, so raising this
    /// costs no extra transport work; set it above a mailbox's count from
    /// mail_list_mailboxes when full coverage matters.
    #[serde(default)]
    pub scan_limit: Option<u32>,
    /// Optional Mail account name; narrows to that account's inbox.
    #[serde(default)]
    pub account: Option<String>,
    /// Optional mailbox name inside `account` (Gmail labels live here, not
    /// in the inbox). Discover via mail_list_mailboxes.
    #[serde(default)]
    pub mailbox: Option<String>,
    /// Optional per-call folder selection: `["Account/Mailbox", …]` (split
    /// on the last '/'). Every entry must lie inside the configured scope —
    /// run mail_config for the current allowlist. `"*"` sweeps EVERY scoped
    /// folder in one call. Takes precedence over account/mailbox.
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
    /// Aggregate instead of listing rows: "sender" groups by sender
    /// address, "subject" by normalized subject (Re:/Fwd: stripped).
    /// Returns {groups:[{key,name,count,first,last,latest_id,
    /// sample_subjects,folders}]} paginated by count — use latest_id with
    /// mail_read for the newest message of a group.
    #[serde(default)]
    pub group_by: Option<String>,
    /// Body previews per row (default false). Each preview costs one Apple
    /// Event plus tokens per row; triage on subjects/senders/groups and
    /// mail_read the rows you actually open.
    #[serde(default)]
    pub include_snippets: Option<bool>,
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
    /// Optional "Account/Mailbox" (as tagged on the search row) to target
    /// the mailbox directly; omit for inbox-first lookup with fallback.
    #[serde(default)]
    pub folder: Option<String>,
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
    /// Target calendar name (default: first calendar; the response
    /// reports which one was used). Discover via calendar_list.
    #[serde(default)]
    pub calendar: Option<String>,
    /// Optional location string.
    #[serde(default)]
    pub location: Option<String>,
    /// Optional notes/description.
    #[serde(default)]
    pub notes: Option<String>,
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
    /// New location (optional).
    #[serde(default)]
    pub location: Option<String>,
    /// New notes/description (optional).
    #[serde(default)]
    pub notes: Option<String>,
    /// Token from a previous requires_confirmation response.
    #[serde(default)]
    pub confirmation_token: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct CalendarDeleteParams {
    /// Event uid from calendar_read.
    pub id: String,
    /// Token from a previous requires_confirmation response.
    #[serde(default)]
    pub confirmation_token: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ContactsSearchParams {
    /// Case-insensitive substring matched against name, organization, and
    /// email addresses. Empty = directory census.
    #[serde(default)]
    pub query: Option<String>,
    /// Page size (default 20, max 100).
    #[serde(default)]
    pub limit: Option<u32>,
    /// Page offset (default 0).
    #[serde(default)]
    pub offset: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
pub struct RemindersReadParams {
    /// Optional reminder list name filter.
    #[serde(default)]
    pub list: Option<String>,
    /// Include completed items (default false).
    #[serde(default)]
    pub include_completed: Option<bool>,
    /// Page size (default 20, max 100).
    #[serde(default)]
    pub limit: Option<u32>,
    /// Page offset (default 0).
    #[serde(default)]
    pub offset: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
pub struct RemindersCreateParams {
    /// Reminder name/title.
    pub name: String,
    /// Target list name (default: default list).
    #[serde(default)]
    pub list: Option<String>,
    /// Due date (ISO 8601), optional.
    #[serde(default)]
    pub due: Option<String>,
    /// Notes/body text, optional.
    #[serde(default)]
    pub notes: Option<String>,
    /// Token from a previous requires_confirmation response.
    #[serde(default)]
    pub confirmation_token: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct RemindersCompleteParams {
    /// Reminder id from reminders_read.
    pub id: String,
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
