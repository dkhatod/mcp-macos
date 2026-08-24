# Changelog

All notable changes. Format follows [Keep a Changelog](https://keepachangelog.com);
versions are semver (0.x: breaking changes bump minor).

## [Unreleased]

## [0.1.9] — 2026-08-24

### Fixed
- **`mail_search(account: …)` crashed with raw `AppleEvent -1728: Can't get
  object`** when passed an email address instead of Mail's display account
  name (agents pass emails constantly — `mail_list_accounts` leads with
  them). Narrowed searches now pre-flight the name against live accounts and
  return `{error, available_accounts}` for one-turn self-correction.
- `mail_config` now reports `server_version` so a live session can prove
  which binary is answering.
- **Under-counted applications in triage runs**: sender groups now carry
  `distinct_subjects` — the number of distinct normalized subjects behind
  one sender (live and index modes; sender grouping only). Big ATS senders
  span many postings from one address (`noreply@mail.amazon.jobs` carries
  every Amazon role); `sample_subjects` caps at 4, so nothing previously
  signalled "this group hides more threads". Agents now have the exact
  drill-down condition.
- Server `instructions` routed triage to plain `mail_search → mail_read`,
  so small local models never discovered `mail_sync`/`source:"index"`, ran
  budget-bound live sweeps over default folders, and skipped drill-down.
  Routing now prescribes sync → indexed census → distinct_subjects-driven
  drill-down. `triage-mail-workflow` prompt rewritten the same way.
- Server version string is now derived from `CARGO_PKG_VERSION` instead of
  a hardcoded literal that had drifted (reported 0.1.7 through 0.1.8).

## [0.1.8] — 2026-08-24

### Added
- **Local mail index** (`state_dir/index.db`, via personai-core 0.2's new
  generic index engine): `mail_sync` pulls message metadata per folder with
  independent commits and date watermarks (1 h clock-skew buffer);
  fingerprint mismatches surface Apple-id reuse. Full mode replaces folder
  partitions wholesale.
- **`mail_search(source: "index")`**: term/folder/date filters, sender and
  subject grouping, and pagination run as local SQL instead of ~210 Apple
  Events per sweep — responses carry `data_as_of`. Default stays `"live"`;
  behavior of existing callers is unchanged.
- **Body cache**: `mail_read` writes through to `mail_bodies`; repeat reads
  are served from disk (`"cached": true`). Pruned to ~200 MB, oldest first.
- FTS5 external-content mirror over subjects/senders maintained by
  triggers (ready for full-text queries; not yet exposed as a tool).

### Changed
- `mcp-macos` now links personai-core as a local path dependency
  (`../personai-core`) until the first crates.io publish.

### Performance (targets; measured at live acceptance)
- Sender census over the whole scope: budget-bound ~25 s live call →
  < 50 ms indexed.
- Delta re-sync after a typical day: < 10 s.
- Repeat `mail_read`: ~0.5–2 s → < 5 ms.

### Fixed
- Regression: a grouped-return refactor dropped the for-loop closer and the
  folder provenance field from generated search scripts, breaking every
  `mail_search` at JXA parse time and nulling group folder tags. Both are
  now guarded by new contract tests (script balance oracle + folder
  provenance assertion) so this failure class cannot recur silently.
- Items-after-test-module warning: `join_rows`/`unwrap_string_payload`
  moved above `mod tests` in `src/util.rs`; clippy is warning-free.

### Added
- `until` parameter on `mail_search` (exclusive upper bound) — bounded
  date windows without post-filtering.
- Row-per-line payloads: array results serialize one record per line so
  client-side output compactors drop whole records instead of slicing a
  single giant JSON line mid-stream (observed with OMP's compactor).
  Valid JSON; zero contract change.
- MCP **prompts**: `triage-mail-workflow` — the census-first recipe is now
  surfaced through the client's prompt UI instead of relying on agents
  following tool descriptions.
- MCP **resources**: the configured state directory is exposed as
  `file://…` resources (`job-apps.json`, `events.jsonl`, …) with
  path-confinement and a 256 KiB read cap, so agents can discover state
  without filesystem access or path guessing.
- Syntax-oracle CI tier: every generated JXA script (all five tool
  groups' builders) is compile-checked with `/usr/bin/osacompile -l
  JavaScript` — which compiles without executing, so no app access or
  TCC prompts — in a new `syntax-oracle` GitHub Actions job on
  macos-latest (`tests/syntax_oracle.rs`).
- Balance-oracle coverage across all tool groups: the paren/brace script
  balance check (now a JS-aware lexer handling strings, template
  literals, comments, and regex literals) runs over messages
  (`read`/`chats`/`send`), contacts (`search`), reminders
  (`lists`/`read`/`create`/`complete`), and calendar
  (`read`/`create`/`update`/`delete`) generated scripts.
- `MCP_MACOS_DEBUG_RAW=1` diagnostic: when `messages_read` fails to
  parse the `{ok,value}` envelope (the live "trailing characters"
  quirk), the raw osascript stdout is re-captured and dumped to stderr
  (first 500 + last 300 chars + total length) so the user can share it;
  zero cost when the env var is unset.

### Changed
- Token diet across every tool payload: caller-supplied `offset`/`limit`
  are no longer echoed back; `truncated:false` is omitted (present only
  when true); dates drop millisecond precision; empty strings/arrays/null
  fields (`snippet`, `organization`, `emails`, `phones`, `body`, `due`,
  `priority`) are omitted instead of emitted; soft-gate notes trimmed to
  `re-invoke with confirmation_token`. Typical census pages shrink ~30%.
- Docs examples updated to the dieted shapes.

## [0.1.7] — 2026-08-23

### Added
- `contacts_search` (read-only): case-insensitive search over name,
  organization, and emails; bulk-fetch + JS filter, page-sized detail
  hydration. Resolves "email Mom" / "text Sam" into concrete addresses.
- Reminders group: `reminders_list_lists`, `reminders_read` (open items
  by default), `reminders_create`, `reminders_complete` — writes
  soft-gated like calendar writes.
- `messages_chats`: chat discovery (identifier, display name, service,
  sample handle, count, last activity) so sends stop guessing handles.
- Calendar completion: `calendar_create`/`calendar_update` accept target
  calendar name (response reports which was used), location, and notes;
  new soft-gated `calendar_delete` marks events deleted (reversible in
  the Calendar UI).

### Fixed
- `messages_read` now emits ONE sqlite3 invocation whose output starts
  with the filtered total followed by the page — total and page are
  consistent by construction instead of counting the whole database.

## [0.1.6] — 2026-08-23

### Fixed
- Scope-rejection errors and `mail_config` no longer show a silently
  truncated folder list: the previous 20-entry cap led an agent to read
  the list as the complete scope and exclude most of the mailbox from
  every later search. Errors now carry mode + full count + complete list
  (`format_scope_hint`), flagging any display truncation explicitly.

### Added
- `mail_config` reports `state_dir` and `state_files` so status/history
  workflows can discover persisted state (e.g. `job-apps.json`) without
  guessing paths.

## [0.1.5] — 2026-08-23

### Added
- Match-all census: `query` is now optional — omitting it (with
  `group_by="sender"`) returns a per-sender census of the target folders in
  one call (`key, count, first, last, latest_id, sample_subjects,
  folders`).
- OR terms: new `any_of` parameter (≤8 terms combined with `query`) matches
  a row when ANY term hits subject or sender, replacing per-keyword
  fan-out.
- Deep scan: `scan_limit` raises the per-mailbox scan depth past the new
  5000 default (max 25 000). The bulk metadata fetch already reads whole
  mailboxes, so depth bounds only post-processing.
- Whole-scope sweep: `folders: ["*"]` expands to every scoped folder for a
  single-call corpus pass under the existing wall-clock budget.
- Named deny-set folders (e.g. `[Gmail]/Spam`, `Trash`) are admitted to
  `mail_search`/`mail_read` when explicitly requested in default-deny-set
  mode; responses carry `scope_note: "denied-folder-explicit"`. Explicit
  allowlists stay strict; sweeps still exclude denied names.

### Changed
- `include_snippets` now defaults to **false**: previews cost one Apple
  Event and one token blob per row; triage on metadata and `mail_read`
  specific rows instead.
- Default scan depth raised from 1000 to 5000 messages per mailbox
  (`SCAN_DEFAULT`), covering real-world deep inboxes without per-call
  tuning.

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
