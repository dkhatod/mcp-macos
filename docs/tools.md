# Tools reference

Every tool returns a single JSON object as text content. List tools are
paginated: `total` (matching items overall), `offset`, `limit` echo, and the
item array. Safety tiers: **auto** executes immediately; **soft-gated**
returns a confirmation payload first.

## Mail

### mail_list_accounts — auto

List configured Mail account names.

```json
{"accounts": ["Google", "dhruvkhatodschool@gmail.com", "Exchange"]}
```

### mail_search — auto

| Param | Type | Notes |
|---|---|---|
| query | string | case-insensitive match against subject or sender |
| account | string? | restrict to one account's inbox |
| since | ISO 8601 string? | only messages received after this instant |
| limit | u32? | page size, default 20, hard max 100 |
| offset | u32? | default 0 |

Returns metadata **only** — never bodies. Scan window: newest 1000 messages
of the target inbox (`SCAN_MAX` in `src/mail.rs`).

```json
{"total": 3, "offset": 0, "limit": 2, "results": [
  {"id": "30575", "subject": "…", "from": "…", "date": "2026-08-21T16:01:33Z",
   "snippet": "first 140 chars"}
]}
```

### mail_read — auto

Read one full message by `id` from a search result.

```json
{"id": "30575", "subject": "…", "from": "…",
 "date": "2026-08-21T16:01:33Z", "body": "full plain-text body"}
```

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
{"total": 5, "offset": 0, "limit": 20, "messages": [
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
{"total": 2, "offset": 0, "limit": 20, "events": [
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
