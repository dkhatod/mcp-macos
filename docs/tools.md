# Tools reference

Every tool returns a single JSON object as text content. List tools are
paginated: `total` (matching items overall), `offset`, `limit` echo, and the
item array. Safety tiers: **auto** executes immediately; **soft-gated**
returns a confirmation payload first.

## Mail

### mail_list_accounts — auto

List configured Mail accounts **with identity details** (display names alone
are often just "Google"/"Exchange").

```json
{"accounts": [
  {"name": "Google", "email": "you@gmail.com", "accountType": "imap", "enabled": true},
  {"name": "Exchange", "email": "you@work.com", "accountType": "exchange", "enabled": true}
]}
```

### mail_list_mailboxes — auto

Enumerate mailboxes per account with message counts. Gmail labels (Work,
Personal, Important…) are mailboxes *outside* the inbox — call this before
`mail_search` when unsure where mail lives. ~2 s for a full sweep.

| Param | Type | Notes |
|---|---|---|
| account | string? | narrow to one account name |

```json
{"mailboxes": [
  {"account": "Google", "name": "INBOX", "count": 120},
  {"account": "Google", "name": "Work", "count": 7},
  {"account": "Google", "name": "All Mail", "count": 23551}
]}
```

### mail_sync — auto

Refresh the local mail index (`state_dir/index.db`) from Mail.app. One
osascript run **per folder** (independent commits ⇒ resumable mid-sweep);
incremental by default — only messages newer than the folder's watermark
minus a 1 h buffer are fetched. `full: true` re-reads whole folders and
replaces their cached rows (fixes moved/deleted drift).

```json
{"synced_per_folder": {"Exchange/Apps": {"scanned": 164, "new": 2,
  "updated": 1, "mismatches": 0}},
 "data_as_of": "2026-08-24T12:00:00.000Z", "duration_ms": 4210}
```

| Param | Type | Notes |
|---|---|---|
| folders | string[]? | `["Account/Mailbox", …]` or `["*"]` = every scoped folder; default = every scoped folder |
| account | string? | keep only that account's folders |
| full | bool? | ignore watermarks, replace partitions wholesale |
| scan_limit | u32? | per-mailbox scan depth, default 5000 |

Run it once per session before index-mode searches; repeat syncs cost
O(new mail), typically seconds.

### mail_search — auto

Returns metadata **only** — never bodies. By default searches the **unified
inbox of ALL accounts**; `account` narrows to one account's inbox, and
`account` + `mailbox` targets a specific mailbox (Gmail labels live there,
not in the inbox). Scan window: newest 1000 messages (`SCAN_MAX` in
`src/mail.rs`).

| Param | Type | Notes |
|---|---|---|
| query | string | case-insensitive match against subject or sender |
| account | string? | restrict to one account (its inbox) |
| mailbox | string? | target this named mailbox within `account` |
| since | ISO 8601 string? | only messages received after this instant |
| folders | string[]? | `["Account/Mailbox", …]` targets, validated against the scope |
| limit | u32? | page size, default 20, hard max 100 — prefer small pages |
| offset | u32? | default 0 |
| group_by | "sender"\|"subject"? | aggregate rows into `{groups:[{key,name,count,first,last,latest_id,sample_subjects,folders}]}` ordered by count; subject mode strips Re:/Fwd: chains |
| include_snippets | bool? | default true; `false` skips body previews for much faster pages |
| source | "live"\|"index"? | default `"live"` (Mail.app Apple Events, 25 s budget). `"index"` queries the local corpus cache from mail_sync instead — instant, no budget; the response carries `data_as_of` so you can judge staleness |

```json
{"total": 3, "results": [
  {"id": "30575", "subject": "…", "from": "…", "date": "2026-08-21T16:01:33Z",
   "snippet": "first 140 chars"}
]}
```

Grouped responses replace `results` with `groups` and add
`total_groups`; follow up with `mail_read(latest_id)`.

### mail_read — auto

Read one full message by `id` from a search result. Pass the row's
`folder` tag to target its mailbox directly; without it the inbox is
tried first, then every mailbox. Bodies are capped at 20 000 chars.

```json
{"id": "30575", "subject": "…", "from": "…",
 "date": "2026-08-21T16:01:33Z", "body": "plain-text body",
 "body_truncated": false}
```

Bodies are cached in `mail_bodies` on first read: a repeat read of the same
message is served from disk and carries `"cached": true`. Cache size is
pruned to ~200 MB (oldest evicted first).

### mail_send — soft-gated

Call with `{to, subject, body}` and no token:

```json
{
  "status": "requires_confirmation",
  "payload": {"to": "mom@example.com", "subject": "Hi", "body": "Hello!"},
  "confirmation_token": "9f2c…",
  "note": "Show this payload to the user; re-invoke mail_send with confirmation_token to execute."
}
```

Re-invoke adding `"confirmation_token"` to execute:

```json
{"status": "sent", "to": "mom@example.com", "subject": "Hi"}
```

Tokens are single-use and expire after 5 minutes; replaying a used token
re-enters confirmation instead of sending again.

## Messages

### messages_read — auto

Reads `~/Library/Messages/chat.db` via `sqlite3` (needs Full Disk Access).
Newest first.

| Param | Type | Notes |
|---|---|---|
| chat | string? | matches chat id, display name, or participant handle |
| limit / offset | u32? | default 20 / hard max 100 |

```json
{"total": 5, "messages": [
  {"from": "+15550001111", "direction": "in", "text": "Call me",
   "date": "2026-08-19T12:00:00Z"}
]}
```

`direction` is `in`/`out`; outgoing rows report `from: "me"`. Attachment-only
messages have empty `text`.

### messages_send — soft-gated

Same flow as `mail_send`: `{to, body}` → confirmation token → execute.
`to` is a participant handle (phone number or email). Action name:
`messages.send`.

## Calendar

### calendar_list — auto

```json
{"calendars": ["Home", "Work"]}
```

### calendar_read — auto

| Param | Type | Notes |
|---|---|---|
| start | ISO 8601 | inclusive range start on event start date |
| end | ISO 8601 | exclusive range end |
| limit / offset | u32? | default 20 / hard max 100 |

```json
{"total": 2, "events": [
  {"id": "E1A2…", "title": "Acme phone screen",
   "start": "2026-08-21T14:00:00Z", "end": "2026-08-21T14:30:00Z",
   "calendar": "Home"}
]}
```

### calendar_create — soft-gated

`{title, start, end}` → confirmation → `{"status": "created", "id": "…"}`.
Created on the first calendar. Action: `calendar.create`.

### calendar_update — soft-gated

`{id, title?, start?, end?}` → confirmation →
`{"status": "updated", "id": "…"}`. Only provided fields change.
Action: `calendar.update`.

## Notifications

### notifications_post — auto

`{title, message, subtitle?}` →

```json
{"status": "posted", "title": "personai"}
```

## Clipboard

### clipboard_get — auto

```json
{"text": "current clipboard contents"}
```

### clipboard_set — auto

`{text}` → `{"status": "set"}`.

## Permissions doctor

### permissions_check — auto, always available

Probes Automation permission for Mail, Calendar, Messages with minimal
read-only scripts.

```json
{"ok": true, "permissions": [
  {"app": "Mail", "ok": true},
  {"app": "Calendar", "ok": true},
  {"app": "Messages", "ok": true}
]}
```

On denial the entry carries `fix` with exact System Settings guidance:

```json
{"app": "Calendar", "ok": false,
 "error": "permission denied: …",
 "fix": "Automation permission missing or revoked. Fix: System Settings > Privacy & Security > Automation > …"}
```

## Error shape

Tool-level failures return HTTP-200 text payloads so agents always get
structured output:

```json
{"error": "osascript timed out after 30s"}
{"error": "permission denied: …", "fix": "System Settings > Privacy & Security > …"}
```
