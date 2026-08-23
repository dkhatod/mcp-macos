# Changelog

All notable changes. Format follows [Keep a Changelog](https://keepachangelog.com);
versions are semver (0.x: breaking changes bump minor).

## [0.1.4] — 2026-08-23

### Added
- Grouped search: `mail_search` accepts `group_by` ("sender" or
  "subject") to collapse matches into per-group counts with `first`,
  `last`, `latest_id`, `sample_subjects`, and `folders`; groups are
  ordered by count and paginated. Subject grouping strips `Re:`/`Fwd:`
  chains before deduping.
- `include_snippets` flag (default true): skipping body previews renders
  pages much faster — previews cost one Apple Event per row.
- `mail_read` accepts the search row's `folder` tag for direct mailbox
  lookup; without it, inbox-first then a defensive sweep across all
  mailboxes. Bodies are capped at 20 000 characters with a
  `body_truncated` flag.

### Changed
- One async mutex per tool group instead of one global lock: slow mail
  sweeps no longer block concurrent calls to other groups.
- Server `instructions` rewritten as routing rules (trigger phrases →
  tools) and tool descriptions lead with use cases; agents route to the
  purpose-built tools instead of shelling out to AppleScript.
- Search lease raised to 60 s with in-loop deadline checks so huge
  unified-inbox scans degrade into partial pages (`truncated`) instead of
  dying at the 30 s transport cap.
- Unknown account names in `mail_list_mailboxes` now echo
  `available_accounts` instead of an empty list.
- README quickstart gained client wiring snippets (OMP, Claude Code,
  Cursor) and a verify-the-mount step.

## [0.1.3] — 2026-08-23

### Added
- Folder-scoped search: `mail_search` accepts an optional `folders`
  parameter (`Account/Mailbox` targets, validated against the configured
  allowlist); multi-folder searches run one JXA pass across all targets
  under a shared wall-clock budget and report `scanned_per_folder` plus
  `truncated` when the budget expires.
- Detailed mailbox listing: per-mailbox message counts, last-activity
  timestamps, and whether each mailbox is inside the configured scope.
- Soft-gated `mail_forward` and `mail_reply`, using the same single-use,
  5-minute confirmation-token flow as sends; both operate on the source
  message so Mail forwards/replies from its account.
- Read-only `mail_config` doctor reporting the effective scope mode, the
  resolved folder list, the default send identity, and the deny set.
- Configuration file (`~/.personai/state/mcp-macos.json`) plus CLI flags
  (`--mail-folders`, `--mail-default-from`) for the mail allowlist and
  default send identity; unknown config entries warn and are dropped,
  never fatal.

### Changed
- `mail_send` accepts an optional `from` address, validated against live
  accounts; when given, outgoing messages are sent from the matching
  account.
- Server `instructions` document the folder-selection model alongside the
  existing pagination and gate guidance.

### Fixed
- The server now executes every tool through JXA (`osascript -l
  JavaScript`). 0.1.x constructed the production transport in default
  (AppleScript) mode, so live tool calls failed with a `-2741` syntax
  error; the mock-transport test suite could not catch it. Real-Mac tests
  already exercised `JxaTransport` directly and kept passing.

## [0.1.2] — 2026-08-23

### Added
- Server-level `instructions` on the `initialize` response: routes clients to
  the purpose-built tools over raw AppleScript, documents pagination (default
  20 / max 100), the soft-gate confirmation-token flow for sends and calendar
  writes, the ungated reads/notifications/clipboard tier, and the
  `permissions_check` first-on-error rule.

### Changed
- Tightened per-tool `description` strings (`mail_search`, `calendar_read`,
  `calendar_create`, `calendar_update`) to surface ISO 8601 parameter formats,
  the confirmation-token gate flow, and example invocations.

## [0.1.0] — 2026-08-22

First release. One stdio binary exposing 14 tools across five Apple apps,
plus a permissions doctor.

### Added
- **Mail** — `mail_list_accounts`, `mail_search` (metadata-only, paginated),
  `mail_read`, `mail_send` (soft-gated).
- **Messages** — `messages_read` (chat.db via sqlite3, needs Full Disk
  Access), `messages_send` (soft-gated).
- **Calendar** — `calendar_list`, `calendar_read`, `calendar_create`,
  `calendar_update` (writes soft-gated; ISO 8601 via native JS dates).
- **Notifications** — `notifications_post`.
- **Clipboard** — `clipboard_get`, `clipboard_set`.
- **Permissions doctor** — `permissions_check`: probes Mail/Calendar/
  Messages automation grants, returns exact System Settings fix on denial.
- **Safety** — soft gate with single-use, 5-minute confirmation tokens;
  per-group token stores (`tokens.<group>.json`) under the state dir.
- **Context discipline** — every list tool paginated (`total`/`offset`/
  `limit`, page ≤ 100); search results never contain bodies.
- **Tool trimming** — `--tools mail,calendar` / `MCP_MACOS_TOOLS`; disabled
  groups hidden from discovery and refuse calls.
- **ToolAnnotations** — read_only/destructive hints on all tools.
- CI: Linux (mock transport) + macOS (real osascript, host-dependent tests
  skip without grants) + tag-triggered binary release.

### Performance (measured, M-series Mac)
- Mail search over a 22k-message inbox: 2–5 s typical via four bulk
  Apple-Event fetches + JS-side filtering.
- Calendar read across 25 calendars: 8–20 s for wide ranges via per-calendar
  bulk `startDate()` fetch.

### Design notes
- All structured results execute through JXA (`osascript -l JavaScript`)
  with a JSON envelope — AppleScript text coercion cannot preserve
  lists/records. See docs/architecture.md §4 and §8.
