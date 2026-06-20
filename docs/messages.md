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

Type definitions live in
[`crates/tau-proto/src/messages.rs`](../crates/tau-proto/src/messages.rs). For
bus events themselves, see [events.md](events.md).

## Handshake

Exchanged when a peer first connects to the harness. The usual extension order
is: peer sends `hello`, then optional `subscribe` and `intercept`; the harness
sends `configure` for supervised extensions; the peer sends `ready` once setup
is done.

- **`hello`** *(peer → harness)* — A participant announces itself just after
  connecting: protocol version, client name, and client kind (`provider` /
  `tool` / `action` / `ui` / `core` / `external`). First message on every
  connection.
- **`subscribe`** *(peer → harness)* — A peer declares which delivered events it
  wants to receive, as a list of selectors (exact name or prefix). Without a
  subscription, only directed traffic reaches the peer.
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
  keys such as `ext_<instance>_cwd`.
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
occurrence or direct snapshot; `replay: true` re-sends a durable historical fact
to a late subscriber.

The protocol no longer has an `ack` input message. The harness does not retain
the runtime event stream in memory; late catch-up for any subscribed peer is
rebuilt from durable session/agent stores and current harness snapshots. Peers
that perform side effects must ignore `deliver` frames with `replay: true`. Some
runtime events, such as `tool.started`, are not durable and are therefore not
replayed.

- **`deliver`** *(harness → peer)* — Harness-owned event delivery envelope.
  `recorded_at` is present for committed runtime deliveries and durable replay
  entries when a timestamp is meaningful. It is absent for synthetic direct
  snapshots.

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
[SECURITY.md](../SECURITY.md#harness-and-extension-boundaries) for the trust
boundary and hardening assumptions.
