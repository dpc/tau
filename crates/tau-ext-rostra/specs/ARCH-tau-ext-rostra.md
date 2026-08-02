# ARCH-tau-ext-rostra: Authenticated Rostra extension

`tau-ext-rostra` is the disabled-by-default built-in `std-rostra` tool
extension. One instance owns one Rostra identity. It uses the exact
`rostra-client = 0.1.0` full client with relay-only Iroh peer transport and
Pkarr HTTPS/DNS discovery; it never enables direct peer-IP transport.

The operator configures a declared Tau secret and a strict reference to it:

```yaml
extensions:
  std-rostra:
    enable: true
    require: false
    secrets:
      rostra_identity_mnemonic: {}
    config:
      identity_mnemonic_secret: rostra_identity_mnemonic
```

The secret is a nonempty 24-word BIP39 Rostra mnemonic. The extension derives
the identity from it and accepts no duplicate public ID. Tau's generic secret
resolver accepts `TAU_SECRET_ROSTRA_IDENTITY_MNEMONIC` with precedence and
removes it from the environment before spawning the extension; when absent, it
uses `<tau-state>/secrets/rostra_identity_mnemonic.yaml`. It passes only the
declared value through `configure`. The mnemonic is never in config, journals,
tool inputs/results, or logging.

The extension opens `<state_dir>/rostra.redb` read-only with the derived
identity. The first signed tool call serializes with every other signed call,
unlocks the client, and then publishes the requested event. This lazy
activation avoids a signature merely from enabling the extension. Activation
can create a local signed node announcement for the persistent Iroh endpoint;
it starts upstream Pkarr identity publication and head merging. Rostra has no
relock operation. The parsed key remains in the extension and in upstream
publisher/merger tasks until client shutdown; its `Copy` key type has no
zeroization guarantee.

The extension exclusively owns `<state_dir>/rostra.redb`, normally under
`<tau-state>/ext/<instance>/`. The owner-private database contains Rostra
graph state, projections, synchronization metadata, a persistent Iroh node
secret, and locally committed signed events. It survives Tau sessions outside
Tau journals. Missing stable state fails startup rather than selecting
Rostra's non-synchronizing memory client. A database bound to another derived
identity fails closed. Operators must choose a distinct instance or move the
old state. This exception for large, independently replicated,
subscriber-unsafe, history-independent state follows
[GATE-event-log-first-extension-state](../../../specs/GATE-event-log-first-extension-state.md).
The authenticated interface and persistence semantics were explicitly approved
under
[GATE-persistence-and-extension-interface-change-approval](../../../specs/GATE-persistence-and-extension-interface-change-approval.md).

The model interface consists of four pull tools—`rostra_status`,
`rostra_list_posts`, `rostra_read_post`, and `rostra_get_profile`—and six
signed tools: `rostra_post`, `rostra_react`, `rostra_follow`,
`rostra_unfollow`, `rostra_update_profile`, and `rostra_vote`. A write result
is canonical JSON text containing the derived identity, full external event
ID, operation, `local_state: "stored"`, and
`publication: "asynchronous_best_effort"`. It confirms only the local durable
transaction, never remote acknowledgement. Posts and replies accept bounded
64 KiB Djot and at most sixteen upstream-valid persona tags. Reactions are
upstream `SocialPost` replies with exactly one supported emoji grapheme, not a
separate event kind; reaction-shaped `rostra_post` replies are rejected and
must use `rostra_react`. Follow exposes the all-persona selector only. Profile
mutation is text-only; avatar delivery, news, shoutbox, raw signed events,
identity generation, and arbitrary follow selectors remain excluded. Votes are
up, down, or clear `SocialVote` state, distinct from emoji reactions.

Lists expose following, locally known two-hop network, and explicit-author
timelines. Every read describes only the configured identity's synchronized
local view; empty and `not_found_local` results never claim global absence.
Pages default to 20 and never exceed 50 records. List excerpts stop near 240
Unicode scalar values, and detailed Djot stops at 64 KiB. Versioned bounded
cursors bind continuation to its timeline and author filter. Exact
`rostra-client-db = 0.1.0` and `rostra-core = 0.1.0` dependencies expose
cursor, selector, event, and key types; all client construction, networking,
and storage access still go through `rostra-client`.

The protocol reader only validates and schedules work. Reads run independently
under an eight-query admission cap. Writes take one mutex across activation and
publication. Every tool has a ten-second model-visible deadline. After request
validation, lane acquisition, and lazy activation, Tau admits the operation for
publication immediately before it calls the operation-specific upstream API.
Cancellation or deadline then suppresses a late terminal and detaches the
extension task: the write lane remains occupied until the upstream call returns.
This deliberately conservative boundary precedes the actual redb transaction;
an entered upstream redb transaction is non-interruptible. Before admission,
cancellation aborts the extension task, although lazy activation may itself
have created its signed node announcement. There is no automatic retry or
durable outbox. A cancelled, timed out, or process-interrupted admitted write
is possibly stored and published; a retry is new intent and can create a second
signed event. Serializing local writes prevents avoidable local forks, but
other devices can still fork and upstream head merging can sign a merge event.
Background publication broadcasts changed heads to self/followers and updates
Pkarr best-effort and periodically.

Stable error categories are `invalid_argument`, `not_ready`, `not_found_local`,
`storage_failure`, `timeout`, and `internal_failure`. Reconfiguration is
rejected while queries remain active. Shutdown waits one second before process
supervision provides the forced-termination boundary. Locked, corrupt, and
identity-mismatched databases fail closed. Upstream schema migrations are
forward-only; operators must stop Tau and back up the database before upgrade,
then restore it and the old binary to roll back.

The trusted extension process receives hostile Rostra content. It bounds and
sanitizes names, bios, persona tags, post bodies, avatar metadata, and
diagnostics; multiline bodies use an external-content wrapper whose closing
sentinel cannot be forged. Signatures authenticate only the Rostra author.
Every remote field is inside an external-content frame. Final output is capped
at 128 KiB; tags also have count and aggregate-byte caps. Rostra can
deserialize a full accepted payload (up to its 16 MiB ingestion limit) before
projection, and open/migration can require temporary disk.

Rostra identities never acquire Tau sender or instruction authority. Tool
policy is the only current authorization boundary: supplying the mnemonic
delegates signing authority to every role allowed these tool names. Tau has no
genuine human per-call confirmation primitive, so a model-supplied confirmation
field would not add authority protection. Operators must limit these tools to
trusted roles. Synchronization updates only the extension database and never
turns remote posts into inbound messages, notifications, activation, or
background model work.
