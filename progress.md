# progress — mcp-macos

Newest entry on top. Read top-to-bottom to reconstruct state; prepend after
each session. `feature_list.json` is the authoritative feature/status map.

## 2026-08-25 — Run 5: Messages integration (index surface)

**State:** Built TDD-first (8 red-green cycles), committed locally, binary
reinstalled to ~/.cargo/bin (0.1.10, version string now parity-guarded by
tests/version_parity.rs).

**Shipped:** shared INDEX_MIGRATIONS composition; messages_sync
(rowid-watermarked, batched, budgeted, resumable); messages_search (FTS5,
bounded snippets, phrase-escaped MATCH); messages_unread;
messages_attachments (metadata only); tapback exclusion in reads; optional
send allowlist. Live verified on real chat.db: 347,467 msgs indexed,
0 shifted rows after full rebuild, search + scoped Dhruv reads working.

**Root causes fixed this run:** doShellScript CR line endings broke every
Messages mapper since inception; separator-counting ambiguity with empty
fields (fixed via sqlite json_object rows); control-char envelope
corruption (sanitizer + JSON.stringify).

**Verification:** cargo test 140 passed / 0 failed / 4 ignored (live
smokes opt-in via env); fmt + clippy -D warnings clean; ./init.sh green.

**Next:** live acceptance items in feature_list.json (mail D9 benchmarks +
messages scoped spot-checks). No publishing.

Newest entry on top. Read top-to-bottom to reconstruct state; prepend after
each session. `feature_list.json` is the authoritative feature/status map.

## 2026-08-24 — Run 4: local memory layer (0.1.8)

**State:** Built and committed locally; crates.io untouched (path dep on
../personai-core until first publish). Live acceptance pending.

**Shipped:** `mail_sync` tool, `mail_search(source:"index")`, cached
`mail_read` — all over a new SQLite corpus index (`state_dir/index.db`)
built on personai-core 0.2's generic index engine. FTS5 mirror maintained
by triggers; fingerprints detect Apple-id reuse; per-folder commits make
sweeps resumable.

**Verification evidence:** cargo test 97 passed / 1 failed
(`real_messages_read` — pre-existing live chat.db parse failure, reproduced
on pre-change code); clippy `-D warnings` clean; fmt clean. New test suites:
tests/mail_index.rs, tests/mail_search_index.rs, tests/mail_body_cache.rs,
tests/mail_server_index.rs.

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
1. DONE 2026-08-22: published — `personai-core 0.1.0` and `mcp-macos 0.1.0`
   live on crates.io; path dep flipped to `"0.1"`; tags v0.1.0 (push when
   remote exists).
2. Create GitHub remotes and push (CI + release job unexecuted; tags push
   triggers first binary release).
3. E2E release gate in OMP (spec §9.4): "check my email for job
   application updates" and "send this summary to Mom" (soft gate →
   confirm). Real sends were deliberately never exercised during this run.
4. Docs deep-dive in progress: architecture.md + reference-codebase review
   brainstorm (see Docs Review phase).

**Known gaps (accepted for v1):**
- `mail_send` / `messages_send` / calendar writes validated by mock + gate
  unit tests only; script shapes follow documented app object models.
- Release binaries: aarch64 only (documented in README).
