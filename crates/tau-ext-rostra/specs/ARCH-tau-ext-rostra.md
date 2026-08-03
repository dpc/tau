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
      post_rate_limit:
        max_events: 10
        window_seconds: 3600
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
old state. Moving or resetting notification state requires a new extension
instance name: retained message IDs are scoped by that publisher name, while
the report-attempt counter is identity-bound. This exception for large, independently replicated,
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
cursors bind continuation to its timeline and author filter. Tau pins
`rostra-client`, `rostra-client-db`, and `rostra-core` to upstream Rostra revision
`045345bd5001776eb338ea2c1f55dd60637db4cd`, whose materialization feed API and
upstream database migration are separately approved external prerequisites. Tau
adds no Rostra table, schema, or migration; all client construction, networking,
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

`post_rate_limit` is an optional strict object and defaults to
`max_events: 10` and `window_seconds: 3600`; both values must be positive.
Posts, replies, and reactions share its runtime-only rolling window, while
follow, unfollow, profile-update, and vote mutations are excluded. After
validation and under the serialized write lane, but before activation or
signing, `std-rostra` prunes expired monotonic timestamps and reserves a slot.
It never rolls an admitted slot back after dispatch, including when the
outcome is uncertain. This is a best-effort local guard, not durable Rostra
accounting: extension restart and reconfiguration reset it, and synchronized
events from other devices do not count. A full window returns
bounded `rate_limited` text and fixed structured
`{"category":"rate_limited","retry_after_seconds":<integer>}` details.

Stable error categories are `invalid_argument`, `not_ready`, `not_found_local`,
`storage_failure`, `timeout`, `rate_limited`, and `internal_failure`. Reconfiguration is
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
trusted roles. `enable_tool_groups: [rostra]` grants the whole interface: the
four reads, six signed writes, and `rostra_notifications`. An operator who
needs a smaller surface must use exact tool-name policy.

`rostra_notifications` is a separately scoped, per-agent opt-in tool with the
strict argument shape `{"enabled": boolean}`. Its extension-owned crash-durable
`<state_dir>/rostra-notifications-v1.cbor` state file contains the configured
Rostra identity, first-enable materialization tip, committed cursor, queued age,
last canonical report time, and the identity-wide `next_report_attempt: u64`
counter. The counter remains when registrations are disabled so publisher-scoped
report IDs never repeat across re-enable or restart; unused sequence holes are
harmless. Startup accepts only its exact schema and configured-identity match;
corrupt or mismatched state fails closed. Canonical `message.delivered` echoes
advance the file checkpoint; policy-file updates never activate the model.
[`SPEC-external-message-reports-and-facts`](../../../specs/SPEC-external-message-reports-and-facts.md)
governs that canonical delivery boundary. The file deliberately does not use
the generic metadata facts governed by
[`SPEC-agent-metadata-requests-and-canonical-facts`](../../../specs/SPEC-agent-metadata-requests-and-canonical-facts.md).
An agent unload disables live delivery without deleting its durable preference.
The extension serializes each policy/checkpoint mutation under one notification
mutex. It writes a same-directory mode-0600 temporary file, syncs it, atomically
renames it, and syncs the mode-0700 parent directory before installing the
candidate in memory. A pre-rename policy mutation retains the previous memory
and disk state and returns failure. Bounded backoff applies only to worker source
and report-enqueue operations; it never retries a live canonical-checkpoint
write. A directory-sync failure after rename keeps memory aligned with the new
visible file, returns failure, and poisons later notification mutations rather
than rolling memory back behind disk. Restart recovery reads the visible
checkpoint when one exists.

The worker subscribes before reconciling, but treats Rostra's lossy broadcast
only as a wake hint. It reads Rostra's bounded durable materialization feed,
takes a fresh direct-followee/persona-selector/follow-epoch snapshot before
each page, excludes self posts and historical syncs,
and produces separate serial batches for each loaded, opted-in agent. The
first enable baseline is the current materialization tip, so existing posts do not
notify. Replay never checkpoints notification state or emits a report. The
`agent.replay_complete` boundary opens reconciliation. A crash clears transient
pending/in-flight state, rescans from the committed durable cursor, and can
duplicate a report without skipping its range. Configured interceptors must not
drop `std-rostra` notification reports: a dropped live report remains in-flight
until restart because this integration adds no live echo retry.

Trailing idle debounce is 30 seconds and maximum batch age is five minutes.
After the first report, a hard five-minute per-agent interval limits canonical
Rostra reports and the payload-free external-message wakes they cause. This
does not promise a five-minute interval between model runs: normal harness
busy-agent batching remains authoritative and may coalesce or delay work.
Count and size limits never bypass those timing limits. A report has at most
32 post previews and 48 KiB of projected text, carries at most 16 KiB of
extension metadata, uses the durable `rostra-batch-v1:<attempt>` message ID,
summarizes omitted posts, and advances the acknowledged materialization range.
Omitted posts remain in the existing Rostra pull-queryable local view rather
than creating an activation backlog.
