# AGENTS.md — mcp-macos

MCP server exposing Apple Mail, Messages, Calendar, Notifications and
Clipboard on macOS. Rust binary on `rmcp` 3.x; behavior gates live in
`personai-core::safety`.

## Ground rules (Karpathy)

1. **Think before coding.** State the approach in two sentences before
   writing code. Resolve ambiguity from `docs/` first, then ask.
2. **Simplicity first.** Boring, small, well-tested. No abstraction until
   the second use case exists.
3. **Surgical changes.** Touch only what the task requires; every changed
   line earns its place.
4. **Goal-driven execution.** Done = `./init.sh` green (fmt, clippy, tests,
   stdio smoke), not "compiles".

## Invariants (violating any is a bug)

- Every tool returns ONE bounded JSON object: list tools paginate
  (`total`/`offset`/`limit`, page ≤ 100) and never include bodies.
- Sends/writes go through `personai-core::safety::SoftGate`. Never execute
  first, confirm after.
- All Apple scripting runs through `personai_core::macos` transports — no
  raw `osascript` calls. User text enters scripts only via `util::js_str`
  (JS) / SQL quote-doubling.
- Tests must pass on Linux via `MockTransport`; real-app tests are
  `#[cfg(target_os = "macos")]` and strictly read-only.

## Where things are (query on demand — don't load everything)

| Need | Read |
|---|---|
| Tool signatures, params, example payloads | `docs/tools.md` |
| Gate mechanics, token stores, TCC permissions | `docs/safety.md` |
| How the system works, request lifecycle, JXA rules, decision records | `docs/architecture.md` |
| Current build state & next step | `progress.md` |
| Feature checklist with evidence | `feature_list.json` |
| One-shot verification command | `init.sh` |

Module map: `src/mail.rs`, `src/messages.rs`, `src/calendar.rs`,
`src/notifications.rs`, `src/clipboard.rs`, `src/permissions.rs` (doctor),
`src/util.rs` (js_str). Each tool group is self-contained; adding a sixth
touches nothing else.

## Conventions

Rust stable, edition 2024 · conventional commits (`feat:`, `fix:`, `docs:`)
· clippy `-D warnings` + rustfmt clean at every commit · spec lives at
`../personai/docs/superpowers/specs/2026-08-20-personai-mcp-suite-design.md`.
