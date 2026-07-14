# SPEC-tau-harness-peer-routing: Best-effort typed peer routing

The harness-owned `message` tool accepts bare `&<session-id>`, typed exact
`&<session-id>/@<agent-id>`, and legacy exact `<session-id>/<agent-id>`
addresses. Bare and exact authority are distinct protocol values. Bare routing
selects exactly one eligible loaded or pending entrypoint agent, preferring idle
over running and least-recently-routed then agent id. Busy eligible agents are
reused. If none exists, only a separately configured `auto_start_role` can create
an endpoint; absence or unavailability of that grant fails without spawn. The
result reports the resolved recipient and whether this delivery started it.

Remote routing uses cooperative same-UID Tau IPC. Callback correlation proves
that the claimed sender harness owns a matching live pending request and binds
sender session/agent, recipient authority, kind, and body. It prevents accidental
misrouting and unintended spend; it is not an ACL against malicious same-user
processes. Peer text remains escaped agent-authored model input, never a
harness/system instruction.

Runtime lookup's initial scan visits at most 128 raw entries and reads at most
16 KiB of regular metadata per candidate. An exhausted scan may ignore only conventional
records whose numeric stem, metadata pid, dead-process result, Unix-socket
shape, and lifecycle-file identities agree, then non-destructively ignore those
revalidated records during one retry. That retry admits at most 128 candidates
while traversing at most 256 raw entries (384 raw visits total) under the same
deadline. Live, liveness-unknown, malformed, mismatched, replaced, ambiguous,
and still-truncated catalogs fail closed. Outbound and inbound socket jobs use
bounded global admission and absolute deadlines. Potentially blocking runtime lookup is
isolated behind a separate 16-job non-queued lease; a stalled storage worker
retains that lease after caller timeout so repeated stalls remain bounded. A
64 KiB message limit bounds accepted peer text. Disconnect and session rollover
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
sender's `AgentMessageSent` projection.

Delivery is best-effort at-least-once, not distributed exactly-once. A crash or
transport loss after receive commit but before acknowledgement is indeterminate;
retry may duplicate the prompt. There is no cross-session WAL, restart resumption,
or deduplication index. Crash ambiguity may duplicate prompts, agents, model work,
and spend. Immediately before receive commit, bare authority, creation-role
membership, provider/model/skill availability, endpoint liveness, and generation
are revalidated. Authority loss reselects once; a second loss fails. Exact routes
never redirect.

Policy and trust are governed by
[DESIGN-peer-entrypoints](../../../specs/DESIGN-peer-entrypoints.md) and
[DESIGN-tau-harness-cross-harness-messaging](DESIGN-tau-harness-cross-harness-messaging.md).
