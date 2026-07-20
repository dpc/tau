# Message reference

Messages are point-to-point protocol traffic between one peer (extension,
provider, or UI client) and the harness. They are **not** bus facts: messages
are never broadcast as events, never written to the durable semantic event logs,
and not matched directly by event subscriptions.

Wire form: `{"message": "<flat_name>", "payload": {...}}` — flat snake_case
names, distinct from events' dotted `category.call` form. The protocol is
directional:

- [`HarnessInputMessage`](../crates/tau-proto/src/messages.rs): messages the
  harness accepts from peers.
- [`HarnessOutputMessage`](../crates/tau-proto/src/messages.rs): messages the
  harness sends to peers.

Bare top-level `Event` values are not valid protocol items. Peers ask the
harness to publish events with `emit`; the harness delivers events to peers with
`deliver`.

## Message-fact publication

The protocol has no special transport-message RPC family. Bridges publish
transient `message.*_reported` events through ordinary `emit`. After a report
commits and broadcasts, the harness stamps the authenticated extension's
required configured instance name and publishes the corresponding canonical
durable `message.delivered`, `message.edited`, `message.deleted`,
`message.reaction_added`, `message.reaction_removed`, or `message.sent` fact.

This keeps point-to-point protocol messages distinct from durable bus facts:
`emit` is a request to publish the transient report; the later canonical
`message.*` event is the persisted semantic record. There is no message-specific
result or synchronous commit acknowledgement. Transport admission, duplicate
suppression, native routing, replies, proactive destinations, remote-send
policy, and retries remain inside the bridge extension.

For a successful remote send, an extension normally emits
`message.sent_reported` before its transient `tool.result_reported`; the harness
later derives both canonical facts. Remote acceptance, canonical fact
persistence, and tool completion are not one transaction and the fact is not a
generic delivery/read receipt.

Type definitions live in
[`crates/tau-proto/src/messages.rs`](../crates/tau-proto/src/messages.rs). For
bus events themselves, see [events.md](events.md).

## Handshake

Exchanged when a peer first connects to the harness. The usual extension order
is: peer sends `hello`, then optional `subscribe` and `intercept`; the harness
sends `configure` for supervised extensions; the peer sends `ready` once setup
is done.

- **`hello`** *(peer → harness)* — A participant announces itself just after
  connecting: protocol version, client name, client kind (`provider` / `tool` /
  `action` / `ui` / `core` / `external`), and optional `capabilities`.
  `message_bridge` lets an authenticated configured extension submit
  `message.*_reported`; a socket/UI peer cannot acquire that authority merely by
  claiming the capability. See
  [SPEC-external-message-reports-and-facts](../specs/SPEC-external-message-reports-and-facts.md).
  This is the first message on every connection.
- **`subscribe`** *(peer → harness)* — A peer declares which historical events
  it wants replayed via `historical_selectors` and which future committed
  events it wants via `live_selectors` (exact name or prefix). Without a
  subscription, only directed traffic reaches the peer. Prefer exact selectors
  listing the concrete events the peer handles; prefix selectors should be used
  only for intentionally generic observers that really need the whole category.
- **`intercept`** *(peer → harness)* — A peer asks to receive matching event
  emissions before they hit the event log, with a priority. Lower priority runs
  first; each interceptor replies with `intercept_reply` to pass, rewrite, or
  drop the offered emission.
- **`ready`** *(peer → harness)* — Sent by an extension after its own startup
  work is done and it is ready to participate in tool dispatch. The harness
  supervisor reacts by emitting the `extension.ready` *event* on the bus so
  subscribers can observe online state without watching every per-component
  pipe.
- **`disconnect`** *(either direction)* — A peer or the harness signals an
  intentional disconnect, with an optional human-readable reason. Distinct from
  a socket dying unannounced. The writer thread also sends this as a best-effort
  sentinel when shutting an extension's stdin.

## Configuration (harness → extension)

- **`configure`** — Sent point-to-point by the harness to one extension
  immediately after that extension's `hello`. Carries the supervised
  extension instance name, whatever the `config: { … }` value was for that
  extension in `harness.yaml`, the extension state directory when available,
  and authorized secrets. In-process extensions don't carry a supervised
  config and receive the empty default. Extension authors should use the
  instance name, not the binary name, when deriving instance-scoped metadata
  keys such as the per-shell-instance workdir key `ext_<instance>_cwd`.
- **`config_error`** *(extension → harness)* — An extension reports back that the
  `configure` payload it received was malformed or unusable; the harness
  surfaces the message just like a `harness.yaml` parse error so the user can
  see why their per-extension config was rejected.

## Emission and interception (peer ↔ harness)

These messages wrap a real bus `Event` while it is entering the harness. They
are messages — not events — because the wrapper is point-to-point protocol
metadata, not the fact subscribers ultimately observe.

- **`emit`** *(peer → harness)* — A peer's request to publish an event. Carries
  the inner event and a `transient` flag controlling whether eligible semantic
  facts should skip durable history. The harness owns source attribution,
  interception, sequencing, persistence, and eventual delivery.
- **`intercept_request`** *(harness → interceptor)* — Directed delivery of an
  emission that has not reached the event log yet. Carries the offered event and
  the same transient metadata.
- **`intercept_reply`** *(interceptor → harness)* — Exactly one response to an
  `intercept_request`: `pass` unchanged, `pass` with a replacement event, or
  `drop`.

## Transport (event delivery)

The harness wraps every event it sends to a peer in `deliver`. Deliveries carry
`EventDelivery { event, replay, recorded_at }`. `replay: false` announces a live
occurrence or a synthetic replay boundary; `replay: true` marks catch-up input
selected by `historical_selectors`, including durable historical facts,
session-scoped restore facts, and current-state snapshots reconstructed for a
late subscriber.

`subscribe` separates `historical_selectors` from `live_selectors`. The harness
replays matching historical facts and current-state snapshots first, sends non-replay
`agent.replay_complete` / `session.replay_complete` boundaries, then releases
live delivery for that connection. Live events selected while catch-up is in
progress are queued for that connection and flushed after the session boundary.

The protocol no longer has an `ack` input message. The harness does not retain
the runtime event stream in memory; late catch-up for any subscribed peer is
rebuilt from durable session/agent stores, session restore facts, and current
harness snapshots. Peers that perform side effects must register live handlers
and ignore `deliver` frames with `replay: true`; restore handlers may opt in to
historical execution facts such as `tool.request` and `tool.started` and to
catch-up snapshots such as `session.agent_loaded` or `harness.session_dir`.
Timer-like extensions should rebuild active state from replayed execution facts
and wait for `agent.replay_complete` before submitting restored overdue internal
prompts.

- **`deliver`** *(harness → peer)* — Harness-owned event delivery envelope.
  `recorded_at` is present for committed runtime deliveries and replay entries
  when a timestamp is meaningful. Synthetic catch-up snapshots receive a
  harness-generated catch-up timestamp; replay boundaries are non-replay
  deliveries.

## Current-session agent roster RPC

Accepted local socket clients use `get_current_session` to request the
harness-owned in-memory current session id. The directed
`current_session_result` is correlation-matched and bypasses event publication,
subscriptions, and persisted runtime metadata.

UI-classified local clients use `get_session_agent_list` to request a shallow,
read-only roster from the harness currently bound to an exact `session_id`.
`current` scope returns current members; `history` also returns previously loaded
members whose latest membership fact is unload. The directed
`session_agent_list_result` echoes the request and session ids and returns either
all rows or one typed error without partial output.

The result carries harness-authoritative lifecycle, runtime, shared navigation
mode, and persistence plus bounded creation and display-name projections. It does
not publish events, scan unrelated agent directories, load agents, or expose
transcript content. Extension and non-UI client connections cannot request or
observe the result. See [Listing and picking session agents](list-agents.md) for
the command, filtering, and stable TSV contract.

## Extension data RPC

Extensions use `extension_data_request` to ask the harness to read or mutate
extension-owned persistent data inside harness-managed state roots. The matching
`extension_data_result` echoes the request id and returns either an operation
value or an error kind/message. Type definitions live in
[`crates/tau-proto/src/messages.rs`](../crates/tau-proto/src/messages.rs); quota
constants currently live in
[`crates/tau-harness/src/harness/extension_data.rs`](../crates/tau-harness/src/harness/extension_data.rs).

Requests choose a storage scope and an operation:

- `session` scope stores data under the extension's current-session root. In
  `tau --ephemeral` sessions this scope is unavailable; requests are rejected
  with a `permission` error before any session directory is created.
- `user` scope stores persistent data under the harness state directory for that
  extension.
- `cache` scope stores cache data under the user cache directory for that
  extension.
- File paths are sanitized relative paths. Absolute paths, `.`/`..`, symlink
  leaves, and symlink ancestors are rejected by the harness.
- Supported operations are whole-file read/write/create/append/delete/rename and
  direct-child directory listing.

Semantic events may still be live/memory-only even when their message wrapper is
not marked `transient`: session `--ephemeral` and per-agent ephemerality both
fold state for the running daemon without necessarily writing the corresponding
session or agent event stream to disk.

The harness enforces per-file and per-directory-list quotas. A request that
exceeds those limits fails with `quota_exceeded`. Current limits are 16 MiB per
file for read/write/create/append operations and 4096 scanned directory entries
for one list operation. These quotas bound individual harness operations; they do
not bound aggregate disk use across many files. See
[SPEC-tau-harness-session-state](../crates/tau-harness/specs/SPEC-tau-harness-session-state.md) for the extension-data trust boundary and hardening assumptions.

## External agent-message RPC

Cross-harness `message` tool delivery uses a dedicated
`external_agent_message` / `external_agent_message_result` RPC instead of
generic `emit`. The sending harness opens the target harness socket discovered
from active-session runtime metadata, sends a `hello` with the narrow external
client kind/name, then sends `external_agent_message` with `request_id`,
`message_id`, a sender-minted per-message capability, sender session/agent
identity, recipient session/agent identity, kind, and message body.

The receiving harness accepts this RPC only from peers that completed that
external-message hello. It validates that the requested recipient session is its
current active session, the message is non-empty, and the recipient agent is live
or pending before starting sender authentication. Authentication runs off the
central harness loop: the receiver connects back to the claimed sender harness
and sends `external_agent_message_auth` with the capability plus the exact bound
message fields, including message body and message/watch-response kind. Only
after an `external_agent_message_auth_result` authorizes those fields does the
receiver publish the harness-owned `agent.message_received` projection. The
result echoes `request_id` and carries an error string on rejection. Raw `emit`
of `agent.message_sent` or `agent.message_received` remains forbidden.
