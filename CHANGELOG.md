# Changelog

All notable changes. Format follows [Keep a Changelog](https://keepachangelog.com);
versions are semver (0.x: breaking changes bump minor).

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
