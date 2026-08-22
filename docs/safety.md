# Safety

Two confirmation tiers, identical behavior across every personai server.
Gates live in `personai-core::safety` and are enforced in code — never in
prompts or agent instructions.

## Auto tier

Reads, searches, notifications, clipboard. Execute immediately, but every
result is bounded: metadata-first, page default 20, hard max 100, `total` +
`offset` for paging. No tool returns an unbounded blob.

## Soft gate (send / write actions)

Covered tools and action names:

| Tool | Action | Token store |
|---|---|---|
| `mail_send` | `mail.send` | `<state-dir>/tokens.mail.json` |
| `messages_send` | `messages.send` | `<state-dir>/tokens.messages.json` |
| `calendar_create` | `calendar.create` | `<state-dir>/tokens.calendar.json` |
| `calendar_update` | `calendar.update` | `<state-dir>/tokens.calendar.json` |

Flow:

1. Agent calls `mail_send {to, subject, body}` with no token.
2. The gate mints a random token, stores `{action, payload, token,
   expires_at, used:false}`, and the tool responds with the **exact payload
   plus the token**. Nothing was sent.
3. The agent shows the payload to the user; the user approves.
4. The agent re-invokes with `confirmation_token`. The gate verifies match +
   TTL + unused, marks it used, and the tool executes.
5. Replay of a used/expired token re-enters step 2 — never a silent resend.

Properties:

- Single-use, 5-minute TTL (`TOKEN_TTL_SECS` in core).
- Store is a human-readable JSON file, written atomically (tmp + rename).
- Each gated group owns its own store file; single-use is enforced per
  group, which is the only scope an action name exists in.

## Hard gate (future)

Irreversible actions — application submissions, deletes — will refuse
without a token minted out-of-band by the user via the CLI:

```
personai-core confirm <action> --secret <s>
```

Reserved for future servers; no mcp-macos v1 tool uses it.

## Permissions (TCC)

macOS gates Apple Events behind per-app Automation grants.

- `permissions_check` probes Mail / Calendar / Messages with minimal
  read-only scripts and reports status plus the fix.
- First run may pop consent prompts — approving them IS the onboarding.
- Revocation at runtime surfaces as `permission-denied` errors carrying a
  `fix` field (see [tools.md](tools.md#error-shape)) instead of cryptic
  AppleEvent codes. Classification lives in
  `personai_core::macos::AppleError::{is_tcc_denial, hint}`.
- `messages_read` additionally needs Full Disk Access for chat.db.

## Extending a gated tool

Three steps, all in the new group's module:

```rust
let payload = json!({ "to": to, "body": body });
match self.check("mygroup.action", &payload, token).await? {
    GateOutcome::Confirm { payload, token } => Ok(confirm_response(payload, token)),
    GateOutcome::Execute => { /* run the real script */ }
}
```

Never execute first and confirm after; never treat a token as a capability
for a different payload.

## Anti-features (binding)

No telemetry. No outbound network from the server. No unbounded results. No
dependency bloat beyond what the features above need.
