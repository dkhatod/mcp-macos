//! Pure mail policy: deny-set matching, config-file loading, and effective
//! scope resolution. No transport dependency — the server layer owns
//! enforcement; this module only derives policy from CLI flags, the config
//! file, and a live account/mailbox snapshot.

use std::path::Path;

/// Mailboxes excluded from the default (no-allowlist) scope. Matching is
/// segment equality — see [`deny_matches`]. Irrelevant when an explicit
/// allowlist is configured.
pub const DEFAULT_DENY: &[&str] = &[
    "Trash",
    "Spam",
    "Junk",
    "Drafts",
    "Outbox",
    "Scheduled",
    "All Mail",
];

/// Whether `mailbox` names a deny-set folder: split on `/`, trim brackets
/// and whitespace from each segment, case-insensitive EQUALITY against a
/// deny word. `[Gmail]/All Mail` matches ("All Mail" segment); `NotTrash`
/// does not.
pub fn deny_matches(mailbox: &str) -> bool {
    mailbox.split('/').any(|seg| {
        let seg = seg.trim().trim_matches(|c| c == '[' || c == ']').trim();
        DEFAULT_DENY.iter().any(|d| d.eq_ignore_ascii_case(seg))
    })
}

/// A configured folder target: `"Account/Mailbox"` (split on the LAST `/`).
#[derive(Clone, Debug, PartialEq)]
pub struct FolderId {
    pub account: String,
    pub mailbox: String,
}

impl FolderId {
    /// Parses `"Account/Mailbox"` (split on the LAST `/`, both sides
    /// trimmed); `None` when either side is empty.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let idx = s.rfind('/')?;
        let (account, mailbox) = (s[..idx].trim(), s[idx + 1..].trim());
        if account.is_empty() || mailbox.is_empty() {
            return None;
        }
        Some(Self {
            account: account.to_string(),
            mailbox: mailbox.to_string(),
        })
    }
}

/// Startup mail policy: explicit folder allowlist (if any) and the
/// convenience default send identity.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MailPolicy {
    pub folders: Option<Vec<FolderId>>,
    pub default_from: Option<String>,
}

impl MailPolicy {
    /// CLI > file > defaults. Missing file yields defaults; malformed file
    /// yields defaults plus a warning. Accepts
    /// `{"mail":{"folders_allow":["A/B",...],"default_from":"x@y"}}`.
    /// Unknown entries are warned about and dropped, never fatal.
    pub fn load(
        cli_folders: Option<&str>,
        cli_default_from: Option<&str>,
        config_path: &Path,
    ) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let mut policy = Self::default();

        match std::fs::read_to_string(config_path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warnings.push(format!(
                "config: unreadable {}: {e}; using defaults",
                config_path.display()
            )),
            Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
                Err(e) => warnings.push(format!(
                    "config: malformed JSON in {}: {e}; using defaults",
                    config_path.display()
                )),
                Ok(value) => apply_file(&value, &mut policy, &mut warnings),
            },
        }

        if let Some(cli) = cli_folders {
            policy.folders = Some(parse_folder_list(cli, &mut warnings));
        }
        if let Some(cli) = cli_default_from {
            let addr = cli.trim();
            if addr.is_empty() {
                warnings.push("config: ignoring empty --mail-default-from".to_string());
            } else {
                policy.default_from = Some(addr.to_string());
            }
        }

        (policy, warnings)
    }
}

/// Applies the parsed config file onto `policy` (file layer only; CLI
/// overrides are applied afterwards by [`MailPolicy::load`]).
fn apply_file(value: &serde_json::Value, policy: &mut MailPolicy, warnings: &mut Vec<String>) {
    let Some(top) = value.as_object() else {
        warnings.push("config: top level must be a JSON object; ignoring file".to_string());
        return;
    };
    for (key, val) in top {
        if key != "mail" {
            warnings.push(format!("config: unknown entry '{key}' ignored"));
            continue;
        }
        let Some(mail) = val.as_object() else {
            warnings.push("config: 'mail' must be an object; ignored".to_string());
            continue;
        };
        for (key, val) in mail {
            match key.as_str() {
                "folders_allow" => match val.as_array() {
                    Some(items) => {
                        let mut list = Vec::new();
                        for item in items {
                            match item.as_str().and_then(FolderId::parse) {
                                Some(id) => list.push(id),
                                None => warnings.push(format!(
                                    "config: ignoring invalid folder entry '{item}'"
                                )),
                            }
                        }
                        policy.folders = Some(list);
                    }
                    None => warnings.push(
                        "config: 'folders_allow' must be an array of strings; ignored".to_string(),
                    ),
                },
                "default_from" => match val.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                    Some(addr) => policy.default_from = Some(addr.to_string()),
                    None => warnings.push(
                        "config: 'default_from' must be a non-empty string; ignored".to_string(),
                    ),
                },
                other => warnings.push(format!("config: unknown entry 'mail.{other}' ignored")),
            }
        }
    }
}

/// Parses the CLI `--mail-folders "<Account/Mailbox,...>"` list.
fn parse_folder_list(cli: &str, warnings: &mut Vec<String>) -> Vec<FolderId> {
    let mut list = Vec::new();
    for entry in cli.split(',') {
        match FolderId::parse(entry) {
            Some(id) => list.push(id),
            None => warnings.push(format!("config: ignoring invalid folder entry '{entry}'")),
        }
    }
    list
}

/// The scope the session actually operates in, resolved against live
/// accounts. One of three modes ([`EffectiveScope::mode`]):
/// - `explicit` — only the validated `folders_allow` entries;
/// - `default-deny-set` — every live mailbox minus [`DEFAULT_DENY`]
///   (no allowlist configured, or the explicit list validated to zero);
/// - `open` — everything (degraded start when enumeration failed, tests).
#[derive(Clone, Debug)]
pub struct EffectiveScope {
    /// Live folder universe: explicit entries, or live-minus-deny in
    /// default-deny-set mode (used by [`EffectiveScope::summary`]).
    entries: Vec<FolderId>,
    explicit: bool,
    open: bool,
}

impl EffectiveScope {
    /// Resolves `policy` against the live snapshot
    /// `[(account_name, [mailbox_names])]`. Unknown entries are dropped
    /// with `scope:` warnings; an explicit list validating to zero live
    /// folders falls back to default-deny-set mode with a warning.
    pub fn validate(policy: &MailPolicy, live: &[(String, Vec<String>)]) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let Some(requested) = &policy.folders else {
            return (Self::deny_set_over(live), warnings);
        };
        let mut entries = Vec::new();
        for id in requested {
            match find_live(live, &id.account, &id.mailbox) {
                Some((account, mailbox)) => {
                    if !entries
                        .iter()
                        .any(|f: &FolderId| f.account == account && f.mailbox == mailbox)
                    {
                        entries.push(FolderId {
                            account: account.to_string(),
                            mailbox: mailbox.to_string(),
                        });
                    }
                }
                None => warnings.push(format!(
                    "scope: dropping unknown folder '{}/{}'",
                    id.account, id.mailbox
                )),
            }
        }
        if entries.is_empty() {
            warnings.push(
                "scope: explicit allowlist has no live folders; \
                 falling back to default-deny-set"
                    .to_string(),
            );
            return (Self::deny_set_over(live), warnings);
        }
        (
            Self {
                entries,
                explicit: true,
                open: false,
            },
            warnings,
        )
    }

    /// Open universe: allows everything (degraded start / tests).
    pub fn open() -> Self {
        Self {
            entries: Vec::new(),
            explicit: false,
            open: true,
        }
    }

    /// Every live mailbox minus [`DEFAULT_DENY`] (kept for `summary`).
    fn deny_set_over(live: &[(String, Vec<String>)]) -> Self {
        let mut entries = Vec::new();
        for (account, mailboxes) in live {
            for mailbox in mailboxes {
                if deny_matches(mailbox)
                    || entries.iter().any(|f: &FolderId| {
                        f.account == account.as_str() && f.mailbox == mailbox.as_str()
                    })
                {
                    continue;
                }
                entries.push(FolderId {
                    account: account.clone(),
                    mailbox: mailbox.clone(),
                });
            }
        }
        Self {
            entries,
            explicit: false,
            open: false,
        }
    }

    /// Whether `account`/`mailbox` may be touched this session. Explicit
    /// mode matches case-insensitively against the validated entries;
    /// default-deny-set mode excludes deny-set names; open allows all.
    pub fn allows(&self, account: &str, mailbox: &str) -> bool {
        if self.open {
            return true;
        }
        if self.explicit {
            return self.entries.iter().any(|f| {
                f.account.eq_ignore_ascii_case(account) && f.mailbox.eq_ignore_ascii_case(mailbox)
            });
        }
        !deny_matches(mailbox)
    }

    /// `"open"` | `"explicit"` | `"default-deny-set"`.
    pub fn mode(&self) -> &'static str {
        if self.open {
            "open"
        } else if self.explicit {
            "explicit"
        } else {
            "default-deny-set"
        }
    }

    /// Human-readable `"Account/Mailbox"` entries, capped at 20.
    pub fn summary(&self) -> Vec<String> {
        self.entries
            .iter()
            .take(20)
            .map(|f| format!("{}/{}", f.account, f.mailbox))
            .collect()
    }
}

/// Case-insensitive lookup of `account`/`mailbox` in the live snapshot,
/// returning the live spellings.
fn find_live<'a>(
    live: &'a [(String, Vec<String>)],
    account: &str,
    mailbox: &str,
) -> Option<(&'a str, &'a str)> {
    for (a, mailboxes) in live {
        if !a.eq_ignore_ascii_case(account) {
            continue;
        }
        for m in mailboxes {
            if m.eq_ignore_ascii_case(mailbox) {
                return Some((a.as_str(), m.as_str()));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- deny_matches (segment-equality rule) ---

    #[test]
    fn deny_matches_trash() {
        assert!(deny_matches("Trash"));
    }

    #[test]
    fn deny_matches_gmail_all_mail_segment() {
        assert!(deny_matches("[Gmail]/All Mail"));
    }

    #[test]
    fn deny_does_not_match_substring_lookalikes() {
        assert!(!deny_matches("NotTrash"));
        assert!(!deny_matches("TrashBin"));
        assert!(!deny_matches("Sent"));
    }

    #[test]
    fn deny_matches_is_case_insensitive_and_trims() {
        assert!(deny_matches("trash"));
        assert!(deny_matches("exchange/JUNK"));
        assert!(deny_matches("Google Mail/ Drafts "));
    }

    // --- MailPolicy::load (precedence, malformed, unknown entries) ---

    #[test]
    fn missing_file_yields_defaults_without_warnings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-macos.json");
        let (policy, warnings) = MailPolicy::load(None, None, &path);
        assert_eq!(policy, MailPolicy::default());
        assert!(warnings.is_empty());
    }

    #[test]
    fn file_values_apply_when_no_cli() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-macos.json");
        std::fs::write(
            &path,
            r#"{"mail":{"folders_allow":["File/Box"],"default_from":"file@x"}}"#,
        )
        .unwrap();
        let (policy, warnings) = MailPolicy::load(None, None, &path);
        assert!(warnings.is_empty());
        assert_eq!(policy.default_from.as_deref(), Some("file@x"));
        let folders = policy.folders.unwrap();
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].account, "File");
        assert_eq!(folders[0].mailbox, "Box");
    }

    #[test]
    fn cli_overrides_file_overrides_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-macos.json");
        std::fs::write(
            &path,
            r#"{"mail":{"folders_allow":["File/Box"],"default_from":"file@x"}}"#,
        )
        .unwrap();

        let (policy, warnings) = MailPolicy::load(Some("Cli/Box"), Some("cli@x"), &path);
        assert!(warnings.is_empty());
        let folders = policy.folders.unwrap();
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].account, "Cli");
        assert_eq!(policy.default_from.as_deref(), Some("cli@x"));

        // CLI folders only: file default_from still applies.
        let (policy, _) = MailPolicy::load(Some("Cli/Box"), None, &path);
        assert_eq!(policy.default_from.as_deref(), Some("file@x"));

        // CLI default_from only: file folders still apply.
        let (policy, _) = MailPolicy::load(None, Some("cli@x"), &path);
        assert_eq!(policy.folders.unwrap()[0].account, "File");
        assert_eq!(policy.default_from.as_deref(), Some("cli@x"));
    }

    #[test]
    fn malformed_json_falls_back_to_defaults_with_config_warning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-macos.json");
        std::fs::write(&path, "{not json").unwrap();
        let (policy, warnings) = MailPolicy::load(None, None, &path);
        assert_eq!(policy, MailPolicy::default());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].starts_with("config:"));
    }

    #[test]
    fn unknown_entries_are_dropped_with_config_warning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-macos.json");
        std::fs::write(
            &path,
            r#"{"bogus":1,"mail":{"folders_allow":["A/B"],"nope":2}}"#,
        )
        .unwrap();
        let (policy, warnings) = MailPolicy::load(None, None, &path);
        assert_eq!(policy.folders.unwrap()[0].account, "A");
        assert_eq!(warnings.len(), 2);
        assert!(warnings.iter().all(|w| w.starts_with("config:")));
        assert!(warnings.iter().any(|w| w.contains("'bogus'")));
        assert!(warnings.iter().any(|w| w.contains("'mail.nope'")));
    }

    #[test]
    fn invalid_folder_entries_dropped_valid_ones_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-macos.json");
        std::fs::write(
            &path,
            r#"{"mail":{"folders_allow":["A/B","NoSlash","/Lead","C/D"]}}"#,
        )
        .unwrap();
        let (policy, warnings) = MailPolicy::load(None, None, &path);
        let folders = policy.folders.unwrap();
        assert_eq!(folders.len(), 2);
        assert_eq!(folders[1].mailbox, "D");
        assert_eq!(warnings.len(), 2);
        assert!(warnings.iter().all(|w| w.starts_with("config:")));
    }

    // --- EffectiveScope::validate ---

    #[test]
    fn no_allowlist_resolves_to_default_deny_set_universe() {
        let live = vec![
            (
                "Acc".to_string(),
                vec!["Inbox".to_string(), "Trash".to_string()],
            ),
            ("[Gmail]".to_string(), vec!["All Mail".to_string()]),
        ];
        let (scope, warnings) = EffectiveScope::validate(&MailPolicy::default(), &live);
        assert!(warnings.is_empty());
        assert_eq!(scope.mode(), "default-deny-set");
        assert!(scope.allows("Acc", "Inbox"));
        assert!(!scope.allows("Acc", "Trash"));
        assert!(!scope.allows("[Gmail]", "All Mail"));
        assert_eq!(scope.summary(), vec!["Acc/Inbox".to_string()]);
    }

    #[test]
    fn explicit_allowlist_resolves_case_insensitively() {
        let policy = MailPolicy {
            folders: Some(vec![FolderId {
                account: "exchange".to_string(),
                mailbox: "apps".to_string(),
            }]),
            default_from: None,
        };
        let live = vec![(
            "Exchange".to_string(),
            vec!["Apps".to_string(), "Inbox".to_string()],
        )];
        let (scope, warnings) = EffectiveScope::validate(&policy, &live);
        assert!(warnings.is_empty());
        assert_eq!(scope.mode(), "explicit");
        // Live spellings win for the summary.
        assert_eq!(scope.summary(), vec!["Exchange/Apps".to_string()]);
        assert!(scope.allows("Exchange", "Apps"));
        assert!(!scope.allows("Exchange", "Inbox"));
    }

    #[test]
    fn unknown_entries_dropped_with_scope_warning() {
        let policy = MailPolicy {
            folders: Some(vec![
                FolderId {
                    account: "Exchange".to_string(),
                    mailbox: "Apps".to_string(),
                },
                FolderId {
                    account: "Ghost".to_string(),
                    mailbox: "Missing".to_string(),
                },
            ]),
            default_from: None,
        };
        let live = vec![("Exchange".to_string(), vec!["Apps".to_string()])];
        let (scope, warnings) = EffectiveScope::validate(&policy, &live);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].starts_with("scope:"));
        assert!(warnings[0].contains("Ghost/Missing"));
        assert_eq!(scope.mode(), "explicit");
        assert!(scope.allows("Exchange", "Apps"));
    }

    #[test]
    fn zero_valid_explicit_list_falls_back_to_default_deny_set() {
        let policy = MailPolicy {
            folders: Some(vec![FolderId {
                account: "Ghost".to_string(),
                mailbox: "Missing".to_string(),
            }]),
            default_from: None,
        };
        let live = vec![("Acc".to_string(), vec!["Inbox".to_string()])];
        let (scope, warnings) = EffectiveScope::validate(&policy, &live);
        assert_eq!(warnings.len(), 2);
        assert!(warnings.iter().all(|w| w.starts_with("scope:")));
        assert_eq!(scope.mode(), "default-deny-set");
        assert!(scope.allows("Acc", "Inbox"));
        assert!(!scope.allows("Acc", "Drafts"));
    }

    // --- summary / open ---

    #[test]
    fn summary_caps_at_20_entries() {
        let live: Vec<(String, Vec<String>)> = (0..25)
            .map(|i| (format!("Acc{i}"), vec![format!("Box{i}")]))
            .collect();
        let policy = MailPolicy {
            folders: Some(
                live.iter()
                    .map(|(a, m)| FolderId {
                        account: a.clone(),
                        mailbox: m[0].clone(),
                    })
                    .collect(),
            ),
            default_from: None,
        };
        let (scope, warnings) = EffectiveScope::validate(&policy, &live);
        assert!(warnings.is_empty());
        let summary = scope.summary();
        assert_eq!(summary.len(), 20);
        assert_eq!(summary[0], "Acc0/Box0");
        assert_eq!(summary[19], "Acc19/Box19");
    }

    #[test]
    fn open_scope_allows_everything() {
        let scope = EffectiveScope::open();
        assert_eq!(scope.mode(), "open");
        assert!(scope.allows("Any", "Trash"));
        assert!(scope.allows("Any", "Whatever"));
    }
}
