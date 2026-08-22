# progress — mcp-macos

Newest entry on top. Read top-to-bottom to reconstruct state; prepend after
each session. `feature_list.json` is the authoritative feature/status map.

## 2026-08-22 — Run 2 complete: all v1 features built and verified

**State:** All plan tasks 6–10 plus spec §11.1 additions implemented,
committed locally (no GitHub remote yet).

**Verification evidence (all green on this machine, macOS arm64):**
- `cargo test`: 10 suites, 35 tests — mock contract tests + real-app
  integration (Mail search over 22k-message inbox, Calendar over 25
  calendars, real chat.db read, live permissions probes).
- `cargo clippy -- -D warnings` clean, `cargo fmt --check` clean.
- stdio smoke: initialize → tools/list returns 14 tools with annotations;
  `--tools mail` returns exactly the mail set; disabled groups refuse calls.

**Key design decisions this run (see git log for detail):**
- Structured results run through JXA (`osascript -l JavaScript`) via new
  `personai-core::macos::{run_jxa_json, JxaTransport}` — the AppleScript
  envelope flattens lists/records and only ever preserved scalars.
- Mail/Calendar reads use bulk metadata fetch + JS filtering; per-item
  `whose()` access re-evaluates queries and times out on large mailboxes.
- Each gated group owns its own token store file
  (`tokens.<group>.json`) under the state dir.

**Blocked / next:**
1. `cargo publish` personai-core, then mcp-macos (needs user crates.io
   token; flip `personai-core` from path dep to version after).
2. Create GitHub remotes and push (CI + release job unexecuted).
3. E2E release gate in OMP (spec §9.4): "check my email for job
   application updates" and "send this summary to Mom" (soft gate →
   confirm). Real sends were deliberately never exercised during this run.

**Known gaps (accepted for v1):**
- `mail_send` / `messages_send` / calendar writes validated by mock + gate
  unit tests only; script shapes follow documented app object models.
- Release binaries: aarch64 only (documented in README).
