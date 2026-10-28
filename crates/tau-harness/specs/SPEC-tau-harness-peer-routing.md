# SPEC-tau-harness-peer-routing: Best-effort typed peer routing

## Record justification

Peer delivery spans runtime discovery, typed socket RPC and callback authentication, target admission and auto-start, durable receive/sender projections, post-commit acknowledgement, and crash cleanup, so no single owning module can state the complete best-effort routing contract coherently.

The harness-owned `message` tool accepts bare `&<session-id>` and the exact-agent
form `[&]<session-id>/[@]<agent-id>`, where `&` and `@` are independently optional.
A plain agent id remains local; a session-only address requires `&`. A bare address
sends to the session's optional canonical main. Bare and exact authority are
distinct protocol values. Main is bound by session membership, not inferred from
an agent's role, name, or lifecycle marker. On demand, the harness may create main
without a prompt from the enabled role with the lowest effective `main_priority`;
no matching-role worker substitutes for it. Missing or unavailable main fails
closed without replacement.
The identity and attachment contract is specified by
[SPEC-session-main-agent](../../../specs/SPEC-session-main-agent.md).
Target rejections report only a fixed `ExternalAgentMessageFailure`
classification. In particular, a reached bare target with no available
inter-session receiver reports that condition separately from a target that
cannot be reached. The sender renders a compact fixed diagnostic that directs
the caller to configure a main role, and never exposes arbitrary
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
alone performs bounded claim-directory traversal. A traversal failure makes the
snapshot incomplete, while an individual failed or timed-out probe preserves
already verified compatible peers and marks the result `scan_truncated`.
Outbound and inbound socket jobs retain bounded global admission and absolute
deadlines. A 64 KiB message limit bounds accepted peer text. Disconnect and
final daemon shutdown cancel live work; generation-tagged completions cannot
enter a retired runtime.
Claim listing and opted-in peer probing share one whole-call deadline. A timed
out or failed opted-in probe is omitted without suppressing successfully
verified peers, and each candidate exchange has a smaller bound within that
whole-call deadline. Candidate scheduling probes the caller's current session
first, then remaining claims in session-id order, so deadline pressure does not
hide the caller behind unrelated failed peers. Cancellation is rechecked after
claim reads and before socket I/O. Exact identity probes are quarantined
diagnostic clients and do not count as completed UIs.

The target admits input before creation or receive publication. Each endpoint
accepts at most 32 queued peer inputs and 256 KiB of queued peer body, including
parked precommit receives, and at most 60 accepted inputs per rolling minute.
Auto-start uses ordinary role/model/required-skill/tool-policy construction and
inherits no remote ancestry, watch, cwd, or transcript. The created live endpoint
is immediately eligible, so concurrent sends coalesce; no second endpoint is
created merely because the first is busy.
After creating main on demand and establishing current-session membership, the
harness assigns it the daemon-lifetime `active` navigation classification and
publishes its complete `agent.stats_updated` projection. This receiving lifecycle
applies whether UI or peer demand triggered creation. Existing recipients and
starts outside that lifecycle retain their navigation behavior. Cold restore
recomputes the normal default.
Peer-created lifecycle purpose is embedded as reserved, non-inheritable metadata
in the immutable harness-owned `AgentStarted` creation fact. The ordinary ordered
creation path commits it before the receive can succeed, and interception cannot
drop or rewrite that protected fact. Clients, extensions, and caller-supplied
initial metadata cannot use the reserved key. Restore loads it before one-shot
extension-query lifecycle classification.
The durable session binding, rather than the lifecycle marker, establishes main
authority. An orphan creation before binding does not become main.

Ordinary remote exact-worker input is reviewed by default. Callback authentication
first binds the original exact worker request, including its body and sender
notice. Only then may target-local `inter_session.review_direct_messages`
redirect the receive to main, retaining the intended worker in typed metadata.
Bare and exact-main messages, local exact messages, CLI input, watches, and control
traffic do not take this path. Disabling review restores exact-worker delivery.
Mainless sessions remain valid, but worker input with review enabled fails closed there.
Reviewed delivery requires a sender protocol capable of representing the review
disposition (major 16 or later); older peers fail before main creation/publication.
The committed main receive and ACK report accepted-for-review, never worker
delivery, and do not await model execution. The sender projection retains both
the actual main and intended worker.

Only the currently available bound main may release an immutable retained review
by its owning journal and accepted event sequence. Release revalidates the original
worker, admission, current main authority, tool ownership, and exact retained facts
at commit. It cannot change sender, body, notices, or target. The worker receives
original peer provenance plus separate approval metadata. A retained linked worker
receipt is the only successful-release idempotency fact; there is no source-side
consumed write. See
[SPEC-agent-message-delivery](../../../specs/SPEC-agent-message-delivery.md).

After validation and endpoint selection, the target enqueues the exact
`AgentMessageReceived` projection but does not acknowledge it. A bounded
sender notice, when returned, is authenticated by the existing callback
together with the body and routing fields. Omission remains accepted for
older-target optional-field degradation; altered supplied text is rejected.
The target then snapshots its own configured incoming
notice into the receive projection. Each notice is limited to 64 KiB, and body
plus both notices consume the existing queued-byte admission budget.
The recipient-local notice is never included in the acknowledgement or sender
projection.
The bounded
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
or global deduplication index. Crash ambiguity may duplicate prompts, agents, model work,
and spend. An ACK or provider effect can also escape after live acceptance and
before worker persistence, then survive a crash that loses the journal fact.
Immediately before receive commit, the harness revalidates main authority when
applicable, provider/model/skill availability, original intended-worker liveness,
endpoint liveness, and runtime generation. Authority loss fails the request;
the harness never selects a replacement main or worker.
