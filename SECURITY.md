# Security Policy

mcp-macos gives AI agents access to personal data (email, messages,
calendar) and communication channels. This document describes the trust
model, per-tool risk, and how to report vulnerabilities.

## Trust model

- The server runs **locally with your user privileges**. It can read
  anything your user can read (subject to macOS TCC grants) and write what
  your user can write.
- **Reads execute without confirmation**; they are bounded (paginated,
  metadata-first) but not permission-gated.
- **Writes and sends are gated**: they return a confirmation payload plus a
  single-use token instead of acting. Execution requires re-invocation with
  that token, which expires in 5 minutes. See
  [docs/safety.md](docs/safety.md).
- There is **no telemetry and no outbound network access**. The binary
  speaks stdio JSON-RPC only — no listening sockets.

## Per-tool risk tiers

| Tier | Tools | Risk notes |
|---|---|---|
| Irreversible | none in v1 | hard-gated actions (submissions, deletes) are reserved for future servers and require out-of-band tokens |
| External side effects | `mail_send`, `messages_send` | sends email/iMessage on your behalf; soft-gated, payload shown before send |
| State mutation | `calendar_create`, `calendar_update`, `clipboard_set` | modify local data; soft-gated / trivially reversible |
| Read-only | `mail_list_accounts`, `mail_search`, `mail_read`, `messages_read`, `calendar_list`, `calendar_read`, `permissions_check` | expose personal data to the calling agent within pagination caps |
| Benign | `notifications_post` | local banner only |

## What this project is NOT for

- Compliance-regulated environments (HIPAA, FedRAMP, …). No audits, no
  attestation.
- Multi-tenant or network-exposed deployments. v1 is stdio-only by design;
  do not wrap it in network bridges without adding auth yourself.

## Reporting a vulnerability

Open a private security advisory via GitHub ("Security" → "Report a
vulnerability") on `dkhatod/mcp-macos`. Please include reproduction steps
and the tool/parameter affected. We aim to acknowledge within 7 days.

## Known limitations

- Confirmation tokens live in plaintext JSON under the state dir
  (`tokens.<group>.json`). They expire in 5 minutes and are single-use;
  protect your state directory like any other credential file.
- `messages_read` reads chat.db directly and requires Full Disk Access for
  the host process — that grant covers more than this server.
- AppleScript/JXA execution inherits TCC grants of the parent process
  (terminal or MCP client host). Revoking those grants revokes the server's.
