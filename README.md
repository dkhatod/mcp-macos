# mcp-macos

A production-grade [Model Context Protocol](https://modelcontextprotocol.io)
server that gives any MCP client safe, structured access to Apple Mail,
Messages, Calendar, Notifications and the Clipboard on macOS.

One Rust binary. One config line. Every tool returns bounded, paginated JSON
designed for small local-model context windows.

```
$ cargo install mcp-macos        # or download a release binary
```

## Quickstart (5 minutes)

1. **Install** the binary (`cargo install mcp-macos`, or grab a prebuilt
   `aarch64-apple-darwin` tarball from Releases).
2. **Grant permissions** (first run only): add the server to your MCP client,
   call `permissions_check`, and approve the Automation prompts. See
   [docs/safety.md](docs/safety.md#permissions).
3. **Wire it into your client.** For Oh My Pi (`mcp.json`):

```json
{
  "mcpServers": {
    "macos": {
      "command": "mcp-macos",
      "args": ["--state-dir", "~/.personai/state"]
    }
  }
}
```

4. **Call a tool.** Example `mail_search` response:

```json
{
  "total": 3,
  "offset": 0,
  "limit": 20,
  "results": [
    {
      "id": "30575",
      "subject": "Your application at Acme",
      "from": "recruiter@acme.com",
      "date": "2026-08-19T10:00:00Z",
      "snippet": "We'd love to move forward..."
    }
  ]
}
```

## Tools

| Tool | What it does | Safety tier |
|---|---|---|
| `mail_list_accounts` | List Mail account names | auto |
| `mail_search` | Search message metadata (never bodies), paginated | auto |
| `mail_read` | Read one full message by id | auto |
| `mail_send` | Send email | soft-gated |
| `messages_read` | Read iMessage/SMS history from chat.db | auto |
| `messages_send` | Send iMessage/SMS | soft-gated |
| `calendar_list` | List calendar names | auto |
| `calendar_read` | Events in a time range, paginated | auto |
| `calendar_create` | Create an event | soft-gated |
| `calendar_update` | Modify an event by uid | soft-gated |
| `notifications_post` | Post a local notification banner | auto |
| `clipboard_get` / `clipboard_set` | Read / write the pasteboard | auto |
| `permissions_check` | TCC doctor: per-app Automation status + fix | auto |

Full request/response examples: [docs/tools.md](docs/tools.md).

## Safety model

- **Auto tier** — reads, searches, notifications, clipboard: execute
  immediately, always bounded (default page 20, hard max 100, `total` +
  `offset` for paging).
- **Soft gate** — sends and calendar writes return
  `{"status": "requires_confirmation", "payload": …, "confirmation_token": …}`
  instead of executing. The agent shows the payload to the user;
  re-invoking with the token executes. Tokens are single-use and expire in
  5 minutes. Gates are enforced in `personai-core::safety`, not in prompts.

Details and the hard-gate roadmap: [docs/safety.md](docs/safety.md).

## Trimming the tool set

Load only what a client needs (spec: each client loads only what it needs):

```
mcp-macos --tools mail,calendar
MCP_MACOS_TOOLS=mail mcp-macos        # equivalent via env
```

Disabled groups are hidden from `tools/list` and refuse `tools/call`.
`permissions_check` is always available.

## Requirements

- macOS (any Apple Silicon or Intel Mac; binaries built for aarch64, x86_64
  via `cargo build --release`).
- **Automation permission** for Mail and Calendar (the `permissions_check`
  doctor walks you through it).
- **Full Disk Access** for the process running the server, for
  `messages_read` (it reads `~/Library/Messages/chat.db` via `sqlite3`).

## Anti-features (binding)

- No telemetry, ever. No outbound network calls from the server.
- No unbounded results: every list tool is paginated and metadata-first.
- Minimal dependencies; single static binary.

## Development

```sh
cargo test          # mock-transport tests run on any OS
cargo test          # on macOS: also exercises real osascript (read-only)
cargo clippy -- -D warnings
```

See [docs/development.md](docs/development.md) for architecture, the JXA
integration notes, and how to add a tool group.

## License

MIT — see [LICENSE](LICENSE).
