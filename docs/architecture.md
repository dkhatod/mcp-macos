# Architecture

How mcp-macos works: components, request lifecycle, the transport layer,
the safety model, and the decisions that got us here. For API-level detail
see [tools.md](tools.md); for contribution mechanics see
[development.md](development.md).

## 1. Design principles

1. **Context discipline.** The bottleneck for local-model agents is context,
   not MCP overhead. Every tool returns one bounded JSON object: metadata
   first, page default 20 / hard max 100, `total` + `offset` for paging.
   Search results never contain bodies; `mail_read` fetches one message.
2. **Safety is code, not prompts.** Sends and calendar writes physically
   cannot execute without a single-use confirmation token minted by the
   server (`personai-core::safety`). Agent instructions can't bypass this.
3. **Boring, testable units.** Each Apple app is one self-contained
   toolset module behind a transport trait. Tests run on Linux against a
   mock transport; real-app tests are read-only and skip when the host
   lacks grants.
4. **No telemetry, no network.** The server makes zero outbound calls.

## 2. Component map

```
 MCP client (OMP, Claude, …)
        │  JSON-RPC over stdio (ndjson codec)
        ▼
 MacosServer ── rmcp 3.x ── #[tool] methods (thin)
   │  │
   │  └── ServerHandler impl — call_tool/get_tool generated;
   │       list_tools hand-written (hides disabled groups)
   ▼
 tokio::sync::Mutex<ServerState>
   ├── MailToolset         src/mail.rs          JXA + Mail.app
   ├── MessagesToolset     src/messages.rs      sqlite3 (chat.db) + JXA send
   ├── CalendarToolset     src/calendar.rs      JXA + Calendar.app
   ├── NotificationsToolset src/notifications.rs StandardAdditions
   ├── ClipboardToolset    src/clipboard.rs     pbcopy/pbpaste subprocesses
   └── SoftGate (per gated group)               personai-core::safety
        │
        ▼
 personai_core::macos
   ├── JxaTransport      osascript -l JavaScript + JSON envelope
   ├── RealTransport     osascript (AppleScript, scalar results only)
   └── MockTransport     canned envelopes for tests (runs on Linux)
```

Responsibilities are strict:

| Layer | Owns | Never does |
|---|---|---|
| `MacosServer` | routing, locking, group enable/disable, annotations | app logic |
| Toolsets | scripts, pagination, gate checks, response shaping | process spawning |
| Transports | subprocess, timeout, stderr classification | know about tools |
| core::safety | token lifecycle (single-use, 5-min TTL) | execute actions |

## 3. Anatomy of a request

`mail_search {"query": "acme", "since": "2026-08-01T00:00:00Z"}`:

1. **Route** — rmcp deserializes arguments into `MailSearchParams`
   (schema generated from the struct), dispatches to the method.
2. **Guard** — `self.enabled.mail`? Disabled groups return
   `{"error": "tool group 'mail' is disabled…"}` without touching apps.
3. **Lock** — `ServerState` mutex (async; held across subprocess awaits).
4. **Build** — `search_expr()` renders a JXA program with all user text
   passed through `util::js_str` (JS string escaping). No string enters a
   script any other way.
5. **Execute** — `run_jxa_json` wraps the expression in an envelope:

   ```js
   (() => {
   try {
     const value = (<expression>);
     return JSON.stringify({ok: true, value: value});
   } catch (e) {
     const num = typeof e.errorNumber === 'number' ? e.errorNumber : -1;
     return JSON.stringify({ok: false, error: {number: num, desc: String(e.message || e)}});
   }
   })()
   ```

   and spawns `osascript -l JavaScript -e <program>` with a 30 s timeout.
6. **Filter & paginate** — the script fetches four bulk arrays (id /
   subject / sender / receivedDate of the newest ≤1000 inbox messages),
   filters in JS, returns `{total, results[page]}`; snippets are fetched
   only for the returned page (≤100 items).
7. **Shape** — the toolset re-checks the contract Rust-side (strips any
   `body` key, truncates to limit) and emits `{total, offset, limit,
   results}`.
8. **Respond** — errors become `{"error": …}` payloads (plus `"fix"`
   guidance when the failure is a TCC denial) so agents always receive
   parseable output.

## 4. The transport layer

### Why JXA and not AppleScript

The original design wrapped AppleScript expressions in a Python-JSON
envelope. Two fatal properties surfaced during live validation:

- AppleScript text coercion flattens lists/records — structured data never
  survived the boundary (only scalars ever did).
- Every dynamic value needed shell-quoting through `do shell script`,
  which broke on real content.

JXA (`osascript -l JavaScript`) has native objects, native
`JSON.stringify`, native ISO dates, and needs no external helpers. The
envelope contract (`{"ok":…}` → parsed `value` or mapped `AppleError`)
is unchanged between transports, so mocks and production stay identical.

AppleScript remains available in core for scalar-only scripts; mcp-macos
uses JXA exclusively.

### Hard-won integration rules

These cost real debugging time against live data (22k-message inbox, 25
calendars) — do not rediscover them:

- **Bulk gets beat specifiers.** `box.messages.subject()` is one Apple
  Event. A `whose(...)` specifier re-evaluates its whole query on *every*
  element access — paging through one times out.
- **whose() results cannot take bulk gets** (`Invalid key form`, `-10002`;
  `Can't get object`, `-1728`). Use `.length` or indexed items only.
- **Dictionary names differ from AppleScript's**: Mail exposes
  `dateReceived`/`content`, not `received date`/`plain text content`.
  Discover with `Object.keys(x.properties())`.
- **Timeouts are the symptom**, not the diagnosis — a hang means an O(n²)
  specifier pattern or a network-backed property (Gmail body fetch).

### Error taxonomy

`personai_core::macos::AppleError`: `AppNotRunning`, `PermissionDenied`,
`Timeout`, `Parse`, `AppleEvent{number, desc}`, `Transport`. Stderr
classification maps `-600`/`not running` and `-1743`/`not allowed`;
through the JSON envelope, AppleEvent `-1743` is recognized as a TCC
denial too (`is_tcc_denial`). Denials carry actionable `fix` guidance
(System Settings path) all the way to the agent.

## 5. Safety model

Soft-gated tools: `mail_send`, `messages_send`, `calendar_create`,
`calendar_update`.

```
 agent                    server                        disk
   │  mail_send{to,…}       │                             │
   ├───────────────────────►│ gate.check("mail.send")     │
   │                        │── mint token, persist ─────►│ tokens.mail.json
   │◄─ {status:"requires_…",│   (atomic tmp+rename)       │
   │    payload, token}     │                             │
   │  (user approves)       │                             │
   ├─ …with token ─────────►│ verify match+TTL+unused     │
   │                        │── consume, persist ────────►│
   │◄─ {status:"sent"}      │── run send script           │
```

- Tokens are random hex, single-use, 5-minute TTL; replaying a consumed
  token re-enters confirmation rather than resending.
- Each gated group owns its store file (`tokens.<group>.json`) under the
  state dir — single-use holds per action-name scope without sharing state.
- Hard-gate (irreversible actions) exists in core (`personai-core confirm
  <action> --secret …`) but no v1 tool uses it.
- `ToolAnnotations` (read_only/destructive hints) are advisory metadata for
  clients; the gates above are the enforcement.

## 6. Performance characteristics

Measured on an M-series Mac, 22k-message Gmail inbox, 25 calendars:

| Operation | Latency | Dominant cost |
|---|---|---|
| `mail_list_accounts` | ~0.3 s | app wake-up |
| `mail_search` (no since) | 2–5 s | 4 bulk AE fetches + snippet fetch per page item |
| `mail_search` (since) | 5–12 s | same, larger scan window |
| `mail_read` | 1–2 s | id lookup whose() scan |
| `messages_read` | 0.3 s | sqlite3 query |
| `calendar_list` | ~0.3 s | app wake-up |
| `calendar_read` (wide range) | 8–20 s | bulk startDate() per calendar |

Levers if this matters more later (from the MacOS-MCP review): parallelize
per-calendar scans (tokio tasks), narrow the default scan window, cache
metadata between calls invalidated by app events. None implemented in v1 —
correctness first, these are additive.

## 7. Threat model (summary)

- **The agent is semi-trusted**: it can read what you can read. Writes need
  human token confirmation; irreversible actions don't exist yet.
- **Injection**: user-controlled strings (query, subject, body, chat
  filter) enter scripts only through escaping helpers with golden tests.
  SQL uses parameter-shaped literals built Rust-side.
- **No amplification**: responses are capped, so a poisoned mailbox can't
  blow up an agent context or the server's memory.
- **Local-only**: stdio transport; no listening sockets, no auth surface.
  If HTTP transport is ever added, adopt the SSRF/IP-allowlist/auth-key
  patterns from the MacOS-MCP review before exposing anything.

Full policy: [safety.md](safety.md). Risk tiers per tool: [SECURITY.md](../SECURITY.md).

## 8. Decision records

| # | Decision | Rationale | Rejected alternative |
|---|---|---|---|
| 1 | Rust + official rmcp SDK | single static binary, near-zero cold start, typed tools | Python/fastmcp (heavy deps, slow start, telemetry pressure) |
| 2 | JXA for structured results | native JSON/dates, survives coercion | AppleScript + python3 envelope (lossy, fragile quoting) |
| 3 | Per-group token stores | single-use semantics without shared mutable gate state | one shared gate file |
| 4 | Hand-written `list_tools` | static router stays; disabled groups hidden | runtime-built routers (complexity) |
| 5 | stdio-only v1 | local threat model, zero auth surface | SSE/HTTP (deferred with security prerequisites) |
| 6 | Read-only CI integration tests | CI runners can't approve TCC prompts; tests skip gracefully | headless automation grant hacks |
| 7 | Metadata-only search | context discipline + privacy (bodies on explicit read) | full-text search returning bodies |
