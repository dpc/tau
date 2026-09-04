# SPEC-tau-harness-peer-routing: Best-effort typed peer routing

## Record justification

Peer delivery spans runtime discovery, typed socket RPC and callback authentication, target admission and auto-start, durable receive/sender projections, post-commit acknowledgement, and crash cleanup, so no single owning module can state the complete best-effort routing contract coherently.

The harness-owned `message` tool accepts bare `&<session-id>` and the exact-agent
forms `&<session-id>/@<agent-id>` and `<session-id>/<agent-id>`. A bare address
sends to the session, whose harness selects exactly one eligible loaded or
pending receiving agent. Bare and exact authority are distinct protocol values.
Session selection prefers idle over running and
least-recently-routed then agent id; busy eligible agents are reused. If none
exists, roles with effective `inter_session_auto_start` are checked in
deterministic configured role order, skipping unavailable roles and models.
Absence or unavailability of a grant fails without spawn. The result reports the
resolved recipient. Successful model tool results report only delivery status and
that resolved recipient; they do not expose recipient selection or auto-start
mechanics.
Target rejections report only a fixed `ExternalAgentMessageFailure`
classification. In particular, a reached bare target with no available
inter-session receiver reports that condition separately from a target that
cannot be reached. The sender renders a compact fixed diagnostic that directs
the caller to set `inter_session_receiver`, and never exposes arbitrary
target-local errors.

Remote routing uses cooperative same-UID Tau IPC. Callback correlation proves
that the claimed sender harness owns a matching live pending request and binds
sender session/agent, recipient authority, kind, and body. It prevents accidental
misrouting and unintended spend; it is not an ACL against malicious same-user
processes. Peer text remains escaped agent-authored model input, never a
harness/system instruction.

Targeted runtime lookup derives one full-BLAKE3 claim and socket pair directly
from the exact session id. An absent or safely unlocked claim linearizes as not
running. A contended claim must contain the exact session identity and its socket
must complete exact-session admission; unavailable, malformed, or mismatched
contended state is incomplete and never falls back to PID or catalog scanning.
Only a daemon holding the session claim may reclaim its stale socket. Listing
alone performs bounded claim-directory traversal and fails wholly on traversal,
deadline, or probe incompleteness. Outbound and inbound socket jobs retain
bounded global admission and absolute deadlines. A 64 KiB message limit bounds
accepted peer text. Disconnect and final daemon shutdown cancel live work;
generation-tagged completions cannot enter a retired runtime.
Claim listing and opted-in peer probing share one whole-call deadline. A timed
out or failed opted-in probe makes the snapshot incomplete rather than exposing
partial results, and cancellation is rechecked after claim reads and before
socket I/O. Exact identity probes are quarantined diagnostic clients and do not
count as completed UIs.

The target admits input before creation or receive publication. Each endpoint
accepts at most 32 queued peer inputs and 256 KiB of queued peer body, including
parked precommit receives, and at most 60 accepted inputs per rolling minute.
Auto-start uses ordinary role/model/required-skill/tool-policy construction and
inherits no remote ancestry, watch, cwd, or transcript. The created live endpoint
is immediately eligible, so concurrent sends coalesce; no second endpoint is
created merely because the first is busy.
After durable creation and current-session membership setup, only that newly
created bare peer-entrypoint endpoint receives the daemon-lifetime `active`
navigation classification and its complete `agent.stats_updated` projection.
Existing bare recipients, exact recipients, and every non-peer start retain
their existing navigation behavior. Cold restore recomputes the normal default.
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
post-acceptance commit hook. Here commit means bounded persistence admission and
the authoritative in-memory fold; it does not wait for filesystem I/O.
Interception rejection, admission failure,
target disappearance, disconnect, or shutdown before commit fails or removes
the continuation without success. Only confirmed acknowledgement permits the
sender's `AgentMessageSent` projection. The same-loop post-commit reaction
transfers queued byte ownership and installs any payload-free live wake before
issuing the ACK, but durable receive commit remains the ACK authority; provider
dispatch and response are not prerequisites. Exact transcript placement and
activation are specified by
[SPEC-agent-message-delivery](../../../specs/SPEC-agent-message-delivery.md).
The append/sync crash boundary is governed by
[SPEC-semantic-journal-writeback-durability](../../../specs/SPEC-semantic-journal-writeback-durability.md).
If receive cancellation removes an already-delivered interception request, the
responder is bypassed until one stale reply is consumed without action. A
replacement registration remains suspended, no timeout applies, and disconnect
resets the connection as specified by
[SPEC-tau-harness-event-processing](SPEC-tau-harness-event-processing.md).

Delivery is best-effort at-least-once, not distributed exactly-once. A crash or
transport loss after receive commit but before acknowledgement is indeterminate;
retry may duplicate the receive occurrence and live model work. There is no cross-session WAL, restart resumption,
or deduplication index. Crash ambiguity may duplicate prompts, agents, model work,
and spend. An ACK or provider effect can also escape after live acceptance and
before worker persistence, then survive a crash that loses the journal fact.
Immediately before receive commit, bare authority, creation-role
membership, provider/model/skill availability, endpoint liveness, and runtime generation
are revalidated. Authority loss reselects once; a second loss fails. Exact routes
never redirect.
