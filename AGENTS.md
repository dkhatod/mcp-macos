# AGENTS.md — mcp-macos

## Ground Rules

1. **Think before coding.** State the approach before writing code.
2. **Simplicity first.** One server, five self-contained tool groups. Adding
   a sixth group must not touch the others. No abstraction until the second
   use case.
3. **Surgical changes.** Touch only what the task requires.
4. **Goal-driven execution.** `cargo test` green on Linux (mock transport)
   before declaring done; macOS CI runner covers real `osascript`
   integration.

## What This Server Is

The first publishable personai MCP server: a single Rust binary, stdio
transport, one config line for any MCP client. Published to crates.io.
Depends on `personai-core` (crates.io; local path dependency on the sibling
repo until the first publish).

## Tool Groups (v1)

| Group | Tools | Safety tier |
|---|---|---|
| Mail | `mail_list_accounts`, `mail_search`, `mail_read`, `mail_send` | reads auto; send soft-gated |
| Messages | `messages_read`, `messages_send` | read auto; send soft-gated |
| Calendar | `calendar_list`, `calendar_read`, `calendar_create`, `calendar_update` | reads auto; writes soft-gated |
| Notifications | `notifications_post` | auto |
| Clipboard | `clipboard_get`, `clipboard_set` | auto |

Each group is a self-contained module (`src/mail.rs`, `src/messages.rs`, …).

Mail details:

- Multi-account aware: `mail_search` accepts an account filter; personal
  config marks which accounts are "job" accounts.
- `mail_search` returns metadata only (subject, sender, date, account,
  snippet, message-id) — never bodies. `mail_read` fetches one message by id.
- `mail_send` is soft-gated (draft shown, token confirmed).

## Context Discipline (hard requirement for every tool)

1. Summary-first, paginated: list tools return metadata, cap results
   (default 20, hard max 100), return `total` + `offset`. No tool may return
   an unbounded blob.
2. State offloads memory: the agent reads its state for the current picture
   and queries apps only for deltas (`since` last check).
3. Bounded decomposition: large tasks decompose into resumable steps.

Safety gates come from `personai-core::safety` — never re-implement them
here.

## Conventions

- Rust stable, Edition 2024, official `rmcp` SDK, `osascript` via
  subprocess. clippy + rustfmt clean; semver from the first release.
- Docs: README (install + 5-minute quickstart), `docs/tools.md` (every tool
  with examples), `docs/safety.md`, `docs/development.md`.
- CI: Linux job (unit + contract tests via mock transport) and a macOS job
  (real `osascript` integration tests).
- Tests: unit per module; contract (tool schemas + mock AppleEvent
  round-trips); integration on the macOS runner.

## Boundaries

- No knowledge of `personai/` (the agent home) or other future servers.
  `personai-core` is the only cross-repo dependency.

Spec: `../personai/docs/superpowers/specs/2026-08-20-personai-mcp-suite-design.md`
(§4). Workspace harness: `../AGENTS.md`.
