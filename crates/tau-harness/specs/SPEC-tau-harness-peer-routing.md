# SPEC-tau-harness-peer-routing: Best-effort typed peer routing

## Record justification

Peer delivery spans runtime discovery, typed socket RPC and callback
authentication, target admission and auto-start, durable receive/sender
projections, post-commit acknowledgement, and crash cleanup. No single owning
module can state the complete best-effort routing contract coherently.

The harness-owned `message` tool accepts bare `&<session-id>` and the exact-agent
forms `&<session-id>/@<agent-id>` and `<session-id>/<agent-id>`. A bare address
sends to the session, whose harness selects exactly one eligible loaded or
pending receiving agent. Bare and exact authority are distinct protocol values.
Session selection prefers idle over running and
least-recently-routed then agent id; busy eligible agents are reused. If none
exists, roles with effective `inter_session_auto_start` are checked in
deterministic configured role order, skipping unavailable roles and models.
Absence or unavailability of a grant fails without spawn. The result reports the
resolved recipient and whether this delivery started it.

Remote routing uses cooperative same-UID Tau IPC. Callback correlation proves
that the claimed sender harness owns a matching live pending request and binds
sender session/agent, recipient authority, kind, and body. It prevents accidental
misrouting and unintended spend; it is not an ACL against malicious same-user
processes. Peer text remains escaped agent-authored model input, never a
harness/system instruction.

Targeted runtime lookup visits at most 4096 raw entries, accepts at most 16 KiB
of regular metadata for each metadata-shaped entry, and reads at most 16 KiB
plus one byte to detect oversize input, while admitting at most 128 records that
claim the requested session. It never deletes runtime files:
pathname identity and PID liveness checks cannot be atomic with PID reuse and a
replacement daemon binding that pathname. Unreachable matching records whose
metadata PID is live or liveness-unknown, ambiguous catalogs, matching-candidate
exhaustion, raw-entry-budget exhaustion, and deadline expiry fail closed; two
successfully probed claimants establish ambiguity even if the scan is otherwise
incomplete. Unreadable, malformed, non-regular, symlinked, or oversized metadata
at a conventional numeric stem leaves uniqueness unresolved when that PID is
live or liveness-unknown. Definitely-dead numeric stems and non-lifecycle-shaped
filenames may be ignored non-destructively. Outbound and inbound socket jobs use
bounded global admission and absolute deadlines. Potentially blocking runtime lookup is
isolated behind a separate 16-job non-queued lease; a stalled storage worker
retains that lease after caller timeout so repeated stalls remain bounded. A 64
KiB message limit bounds accepted peer text. Disconnect and session rollover
cancel live work, and generation-tagged completions cannot enter a replacement
session.

The target admits input before creation or receive publication. Each endpoint
accepts at most 32 queued peer inputs and 256 KiB of queued peer body, including
parked precommit receives, and at most 60 accepted inputs per rolling minute.
Auto-start uses ordinary role/model/required-skill/tool-policy construction and
inherits no remote ancestry, watch, cwd, or transcript. The created live endpoint
is immediately eligible, so concurrent sends coalesce; no second endpoint is
created merely because the first is busy.
Peer-created lifecycle purpose is embedded as reserved, non-inheritable metadata
in the immutable harness-owned `AgentStarted` creation fact. The ordinary ordered
creation path commits it before the receive can succeed, and interception cannot
drop or rewrite that protected fact. Clients, extensions, and caller-supplied
initial metadata cannot use the reserved key. Restore loads it before one-shot
extension-query lifecycle classification.
The first receive rechecks that the durable `AgentStarted` contains both the
selected creation role and marker; missing creation persistence fails and cleans
up the live reservation instead of acknowledging.

After validation and endpoint selection, the target enqueues the exact
`AgentMessageReceived` projection but does not acknowledge it. A bounded
in-memory, generation-bound continuation acknowledges only from the
post-persistence commit hook. Interception rejection, persistence failure,
target disappearance, disconnect, or rollover before commit fails or removes
the continuation without success. Only confirmed acknowledgement permits the
sender's `AgentMessageSent` projection. The same-loop post-commit reaction
transfers queued byte ownership and installs any payload-free live wake before
issuing the ACK, but durable receive commit remains the ACK authority; provider
dispatch and response are not prerequisites. Exact transcript placement and
activation are specified by
[SPEC-agent-message-delivery](../../../specs/SPEC-agent-message-delivery.md).
If receive cancellation removes an already-delivered interception request, the
responder is bypassed until one stale reply is consumed without action. A
replacement registration remains suspended, no timeout applies, and disconnect
resets the connection as specified by
[DECISION-interceptor-stale-reply-suspension](../../../specs/DECISION-interceptor-stale-reply-suspension.md).

Delivery is best-effort at-least-once, not distributed exactly-once. A crash or
transport loss after receive commit but before acknowledgement is indeterminate;
retry may duplicate the receive occurrence and live model work. There is no cross-session WAL, restart resumption,
or deduplication index. Crash ambiguity may duplicate prompts, agents, model work,
and spend. Immediately before receive commit, bare authority, creation-role
membership, provider/model/skill availability, endpoint liveness, and generation
are revalidated. Authority loss reselects once; a second loss fails. Exact routes
never redirect.

Policy and trust are governed by
[DECISION-tau-harness-cross-harness-messaging](DECISION-tau-harness-cross-harness-messaging.md).
