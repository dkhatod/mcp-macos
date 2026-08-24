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
3. **Wire it into your MCP client.** An installed binary does nothing until
   your client's config registers it — the server only mounts at session
   start. Give it a distinctive name: if another server already uses a
   near-identical one (e.g. the unrelated `macos-mcp` desktop-automation
   npm package), you may believe yours is connected when it isn't.

   Oh My Pi (`~/.omp/agent/mcp.json`, or project `.omp/mcp.json`):

   ```json
   {
     "mcpServers": {
       "mcp-macos": {
         "command": "/Users/YOU/.cargo/bin/mcp-macos",
         "args": ["--state-dir", "~/.personai/state"]
       }
     }
   }
   ```

   Claude Code: `claude mcp add mcp-macos -- ~/.cargo/bin/mcp-macos`

   Cursor / Windsurf / generic (`mcp.json`): same shape as the OMP block
   above under `"mcpServers"`.

4. **Verify the mount before real use.** Start a fresh session (servers do
   not hot-load into running ones) and confirm your agent sees the tools:

   - OMP: `/mcp list` shows the server; ask the agent *"list your
     mail-related tools"* — expect `mail_search`, `mail_read`,
     `mail_list_accounts`.
   - Any client: same question works; if the answer describes AppleScript
     instead, the mount failed — check the config path and name collisions,
     then restart the session.

5. **Call a tool.** Example `mail_search` response:

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
| `mail_list_accounts` | List accounts with identity details (email, type) | auto |
| `mail_list_mailboxes` | List mailboxes per account with counts | auto |
| `mail_search` | Search message metadata (never bodies) across all accounts by default; optional account/mailbox narrowing; `group_by="sender"/"subject"` aggregates rows into counts with `latest_id` for triage; `include_snippets=false` for fast pages | auto |
| `mail_read` | Read one full message by id | auto |
| `mail_send` | Send email | soft-gated |
| `messages_chats` | List chats (identifier, name, service, handle, last activity) for send addressing | auto |
| `messages_read` | Read iMessage/SMS history from chat.db | auto |
| `messages_send` | Send iMessage/SMS | soft-gated |
| `contacts_search` | Search Contacts: name/org/email → id, emails, phones (read-only) | auto |
| `calendar_list` | List calendar names | auto |
| `calendar_read` | Events in a time range, paginated | auto |
| `calendar_create` | Create an event (named calendar, location, notes) | soft-gated |
| `calendar_update` | Modify an event by uid | soft-gated |
| `calendar_delete` | Delete an event by uid (reversible in Calendar) | soft-gated |
| `reminders_list_lists` / `reminders_read` | List reminder lists / read open reminders | auto |
| `reminders_create` / `reminders_complete` | Create / complete a reminder | soft-gated |
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
Groups: `mail`, `messages`, `calendar`, `contacts`, `reminders`,
`notifications`, `clipboard`. `permissions_check` is always available.

## Requirements

- macOS (any Apple Silicon or Intel Mac; binaries built for aarch64, x86_64
  via `cargo build --release`).
- **Automation permission** for Mail and Calendar (the `permissions_check`
  doctor walks you through it).
- **Full Disk Access** for the process running the server, for
  `messages_read` (it reads `~/Library/Messages/chat.db` via `sqlite3`).
- **Troubleshooting** — `messages_read` parse errors ("trailing
  characters"): re-run with `MCP_MACOS_DEBUG_RAW=1` and share the captured
  HEAD/TAIL stderr dump.

## Output format note

Array payloads are emitted **one record per line** (still valid JSON).
Client output-compaction features that truncate long single lines will
then drop whole records instead of mid-record fragments — pair this with
a generous tool-output threshold in your client config for best results.

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
