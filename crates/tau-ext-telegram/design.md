# Design decisions

This file records major design decisions currently embodied by this directory's code, and how authoritative each decision is. It is not an architecture overview, ADR log, todo list, roadmap, implementation guide, or changelog.

## Telegram updates use Bot API long polling

Status: confirmed, 2026-07-05, user

Telegram inbound delivery uses the Bot API `getUpdates` endpoint with long
polling. This is the protocol-provided pull delivery mode for Telegram bots and
fits this extension's architecture because it keeps all network activity as
outbound HTTP from the extension process.

This is distinct from local sleep-loop polling inside Tau. Local waits for Tau
state, shutdown, channels, timers, or other in-process conditions should be made
reactive instead of implemented as periodic sleep loops.

## Telegram update streams are Tau-locked per state root

Status: unconfirmed

Telegram's Bot API `getUpdates` cursor is singleton state for one API base plus
bot token. Before this extension polls or drains that stream, Tau takes an
advisory exclusive OS lock scoped to the stream identity so another Tau process
sharing the same Tau state root fails closed instead of racing update offsets.

The lock key uses a non-secret BLAKE3 fingerprint over API base plus bot token.
Lock metadata may include owner process details, API base, and that fingerprint,
but never the raw bot token. The lock is advisory and local to processes that use
the same Tau state/ext root; separate users, containers, or explicitly separate
Tau state roots are outside this coordination scope.

Telegram webhooks are mutually exclusive with `getUpdates`. Starting active
polling checks `getWebhookInfo` after acquiring the local lock and fails visibly
if a webhook is configured, without deleting the webhook or dropping pending
updates. Subsequent registrations join the already-owned stream. Because
Telegram does not expose active long-poll ownership, HTTP 409 `getUpdates`
conflicts are treated as reactive contention diagnostics: the extension surfaces
a warning and clears active registrations so agents do not believe they still own
the update stream.

## Telegram tool names are instance-namespaced outside `std-telegram`

Status: unconfirmed

Tau tool names are global within one harness prompt/routing surface. Multiple
Telegram extension instances with distinct bot tokens therefore must not all
publish `telegram_register` and `telegram_send`. The built-in `std-telegram`
instance keeps those historical names and group `telegram`; any other instance
derives a collision-free namespace from the configured extension instance name
by escaping underscores as `__` and hyphens as `_d`, unless
`config.tool_namespace` explicitly sets a valid ASCII tool namespace. The
resulting tools are `<namespace>_register` and `<namespace>_send`, with group
`<namespace>`.

The namespace is computed from initial configuration before `Ready` and cannot
change on runtime reconfiguration because tool declarations are startup
declarations. Per-token update-stream locking remains independent of tool
namespacing, so accidental token reuse between differently named instances still
fails closed.

## Multi-session Telegram uses a single-token gateway

Status: accepted, 2026-07-05, issue tau-agent-549e

A single Telegram bot token can serve multiple active Tau sessions only through a
single local gateway owner for that Telegram update stream. Telegram's
`getUpdates` cursor, webhook state, and conflict behavior are global to one Bot
API base plus bot token, so intentionally sharing a bot by running one poller per
Tau session is not supported. The accepted architecture is one gateway daemon per
bot token/API base, plus lightweight per-session gateway-client sidecar
extensions.

The gateway owns the bot token, Telegram client, `getWebhookInfo` preflight,
`getUpdates` loop, update offset/dedup state, stream-owner advisory lock,
allowlist enforcement, chat/session routing, command replies, and Telegram
`sendMessage`. The gateway-client sidecar runs inside each participating harness,
does not read the bot token, does not poll Telegram, exposes the existing
register/send tools (`telegram_register` and `telegram_send`, or that instance's
namespaced equivalents), tracks its local `session_id` and registered agents, and
registers live `(session_id, agent_id)` targets with the gateway over private
local IPC.

Inbound Telegram text flows through the gateway and then the selected sidecar.
The gateway accepts only allowed Telegram users/chats, parses routing commands
such as `/sessions`, `/agents`, `/select-session`, `/select`, `/to`, and
`/where`, resolves an explicit live `(session_id, agent_id)`, and sends a bounded
submit request to that target's sidecar. The sidecar submits to its own harness
with `extension.prompt_submit_request`; the harness remains responsible for
validating the loaded agent and recording normal `agent.prompt_submitted` facts.
The gateway and sidecar must not forge transcript prompt facts directly or route
Telegram prompts through `external_agent_message`.

Outbound replies keep the current safety invariant. An agent calls the local
send tool with message text only. The sidecar verifies that the caller is a
registered local agent and forwards the request to the gateway; the gateway maps
that registered agent to the configured or selected Telegram reply context and
calls `sendMessage`. The model never supplies an arbitrary Telegram `chat_id`.
If future multi-chat support is added, destinations should be gateway-minted
conversation contexts selected by routing policy, not raw chat ids chosen by the
model.

The gateway registry is authoritative but live-only: sidecar connection id,
`session_id`, `agent_id`, display name, optional safe session label, tool
namespace, registration time, selected chat/reply policy, and heartbeat state.
Registrations are removed on sidecar disconnect, heartbeat expiry, session
shutdown, agent unload, or explicit unregister. Reconnecting sidecars must
re-announce their current session and registered agents after gateway restart.

Security requirements for gateway mode:

- only the gateway reads or logs decisions about the bot token; raw tokens and
  full Bot API URLs must not appear in logs, diagnostics, socket names, or lock
  metadata;
- legacy local-poll mode and gateway mode share the same stream lock scope and
  fail closed on accidental same-token reuse;
- non-allowlisted Telegram users/chats are rejected before routing or side
  effects, and group/supergroup chats require explicit `chat_id` configuration
  until a separate group trust model exists;
- local IPC uses a private same-user runtime directory and versioned messages;
- Telegram source labels, display names, aliases, and session labels are bounded
  and sanitized before inclusion in submitted prompt text or command output;
- `/sessions` and related listings expose short gateway-local aliases and
  optional configured labels, not full project paths or unnecessary local
  session details by default;
- message sizes, command output, pending sidecar deliveries, and outbound send
  rate are bounded.

Gateway offset state should become small and durable: stream identity hash, next
Telegram update offset or last processed update id, chat link/selection state,
and optional recent update ids for duplicate suppression. Persisting after an
update is handled can redeliver a message after crash; persisting before handling
can lose it. Prefer possible duplicate delivery over silent loss.

Legacy `std-telegram` local-poll mode remains valid for a single session or
separate bot tokens. Gateway mode is the recommended path when one bot token is
intended to serve multiple active Tau sessions. No core Tau protocol change is
required for the sidecar MVP; if direct gateway-to-harness routing is added
later, it should use a narrow external prompt submission RPC with structured
results rather than overloading agent-to-agent messaging.
