# Development

Prerequisites: Rust stable (edition 2024, MSRV 1.88). macOS for integration
tests; Linux runs the full unit + contract suite on the mock transport.

## Commands

```sh
cargo test                      # all suites; real-app tests auto-enable on macOS
cargo test --test mail          # one group
cargo clippy -- -D warnings     # CI gate
cargo fmt --check               # CI gate

# stdio handshake smoke:
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2026-07-28","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
  | cargo run -q
```

## Architecture in one screen

```
MCP client ──stdio──► MacosServer (rmcp #[tool_router])
                        │ tokio::sync::Mutex<ServerState>
                        ├─ MailToolset<Transport>          src/mail.rs
                        ├─ MessagesToolset<Transport>      src/messages.rs
                        ├─ CalendarToolset<Transport>      src/calendar.rs
                        ├─ NotificationsToolset<Transport> src/notifications.rs
                        └─ ClipboardToolset                src/clipboard.rs
                              │
                              ▼
                 personai_core::macos::{JxaTransport, MockTransport}
```

- **Toolsets own behavior**, transports own execution. Every toolset is
  generic over `AppleTransport`, so unit tests run anywhere with
  `MockTransport` (canned JSON envelopes, recorded scripts).
- **`ServerHandler` is hand-written** (`#[tool_handler]` on
  `impl rmcp::ServerHandler for MacosServer`) only because `list_tools`
  filters disabled groups; everything else (`call_tool`, `get_tool`,
  `get_info`) is macro-generated from the router.

## JXA integration notes (hard-won)

- `doShellScript` output uses **CR** line endings; split on `/\r\n|\r|\n/`.
  Splitting on `\n` alone made every live messages_read return total:0.
- Emit rows via sqlite `json_object()` and parse each line as JSON —
  hand-rolled `'|||'` separators are ambiguous when fields are empty.
- Sanitize stored text (`[\u0000-\u001F\u007F\u2028\u2029]`) before it
  reaches any single-line JSON payload.

All Apple scripting goes through `osascript -l JavaScript` and
`personai_core::macos::run_jxa_json`, which wraps an expression in a
`JSON.stringify({ok, value|error})` envelope. Rules the codebase already
paid for:

1. **Bulk property gets over specifiers** — `box.messages.subject()` is one
   Apple Event for the whole collection. A `whose(...)` specifier
   re-evaluates its query per element access; page through it and Mail times
   out. Fetch arrays once, filter/slice in JS.
2. **`whose()` results cannot take bulk gets** (`Invalid key form`,
   `Can't get object`). Use them only for `.length` or single indexed items.
3. **Dictionary names differ from AppleScript's**: Mail uses `dateReceived`
   and `content`; Calendar uses `uid/summary/startDate/endDate`. Discover
   with `Object.keys(app.x[0].properties())` before writing a script.
4. **User text enters scripts only via `util::js_str`** (JS string literal);
   SQL text uses doubled single quotes (`sql_str`). Both have golden tests.
5. **Real-app validation is part of done**: every builder has been run
   against live apps (22k-message inbox, 25 calendars) before shipping.

## Adding a tool group

1. New module `src/<group>.rs`: `<Group>Toolset<T: AppleTransport>` with
   `new()` + `with_gate(token_store)` if any action needs confirmation.
2. Contract tests first (`tests/<group>.rs`) against `MockTransport`;
   add a `#[cfg(target_os = "macos")]` read-only real-app test.
3. Register tools in the inherent impl block (`#[tool]` +
   `Parameters<...>` schema struct), add the group to `EnabledTools`
   (`allows` + `parse` + guards).
4. Extend the contract test asserting trimmed discovery if the group can be
   disabled.
5. Document in [tools.md](tools.md) and the README table.

## CI

`.github/workflows/ci.yml`:

- **linux** — fmt, clippy `-D warnings`, full test suite (mock transport;
  macOS-gated tests compile out).
- **macos** — same plus real `osascript` integration tests. Strictly
  read-only: no sends, no calendar writes, clipboard roundtrip restores the
  prior content.
- **release** (tags `v*`) — builds release binary, packages
  `mcp-macos-aarch64-apple-darwin.tar.gz` onto the GitHub release.
  crates.io publishing stays a manual `cargo publish` by a maintainer with
  the token.

## Release checklist

1. Bump version in `Cargo.toml`.
2. Update README/docs if tools changed.
3. `cargo publish --dry-run` then `cargo publish`.
4. Tag `vX.Y.Z` and push — CI attaches binaries to the GitHub release.
