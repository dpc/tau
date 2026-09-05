# ARCH-external-message-boundary: External message boundary

External-message bridges publish transient `message.*_reported` events through
ordinary extension event emission. After report commit and broadcast, the
harness stamps the authenticated extension's stable configured publisher ID
and publishes the corresponding immutable durable `message.*` fact. It does not
own transport registration, admission, routing, reply authority, or send
completion.
The exact shared behavior is specified by
[SPEC-external-message-reports-and-facts](SPEC-external-message-reports-and-facts.md);
the underlying publication contract is
[SPEC-peer-event-publication](SPEC-peer-event-publication.md).

Extension-provided labels or payload can never claim `HumanUi`,
harness-internal, authenticated Tau-agent, or another extension instance
authority. External payload always remains untrusted content; transport
authentication, identity assurance, allowlist/lax routing policy, and actionable
native routes remain extension-local.

## Boundary map

| Path | Trust model | Required focus |
| --- | --- | --- |
| Configured local extension IPC | Trusted same-user executable; limited protocol authority | Lifecycle, source ownership, routing identity, config, collisions, accidental-failure isolation |
| Inter-session/inter-harness IPC | Cooperative same-UID, best-effort coordination | Correlation, bounded model-spend admission, stale-generation rejection, duplicate tolerance |
| External adapter/network input | Untrusted payload and sender metadata | Adapter authentication where applicable, strict parsing/bounds, identity separation, instruction isolation |

External content remains untrusted when an extension proxies it, but that does not
turn the configured local extension stream into a hostile-process sandbox. See
[`SECURITY.md`](../SECURITY.md) for the review rule.

Bridge-issued reply and reaction references select extension-private runtime
state; generic consumers treat them as opaque data, and replay does not recreate
their authority. Durable facts may contain bounded inert sender, conversation,
message, and publisher-defined metadata visible to authorized event subscribers,
but never transport credentials or bearer route values.

The generic infrastructure does not deduplicate, order, resolve, authorize, or
mutate message facts. Each successfully emitted fact is a distinct immutable
occurrence. Adapters may apply bounded transport-local retry suppression. Slack
keeps only a process-local recent native-id cache, so cache eviction, restart, or
races may duplicate delivery.

Valid committed facts project to an escaped `<message event="…">` boundary
with opaque message/sender references and optional display, authentication, and
configured-alias metadata as specified by
[SPEC-external-message-reports-and-facts](SPEC-external-message-reports-and-facts.md).
Incoming facts immediately create a payload-free live wake; branch-applicable
transcript placement and provider dispatch may follow later. Replay reconstructs
context without creating a runtime wake. Publisher metadata and message
content remain untrusted and confer no identity, instruction, route, tool, or
egress authority.

## Cross-harness messages

Cross-harness agent messages are local IPC between Tau harness daemons for the
same user. All of that user's harness instances are cooperative and mutually
trusted; Tau does not try to stop one harness from deliberately using another
harness's ordinary UI socket. Together they prevent agent-controlled configured
components from mutating unrelated Tau state and hide runtime socket discovery by
default. A persistent harness exposes its real Tau state tree recursively
read-only unless `tau_state_access: hidden` is selected explicitly. Hostile
same-UID, procfs, and ptrace containment remain outside this boundary.
Harness-owned discovery and messaging retain direct runtime socket access.
Dedicated external-message connections may send only their cross-harness
message, authentication, and session-probe RPCs; they cannot fall through to UI
or Action handlers. They use a dedicated external-message RPC rather than generic event
`emit`; extensions and ordinary UI clients cannot publish harness-owned
`agent.message_sent` / `agent.message_received` projections directly. The
receiving harness performs only bounded syntax and claimed-session checks before
it authenticates caller-supplied sender identity plus message/watch-response kind
by calling back to the claimed sender
harness with a sender-minted per-message capability bound to the message body and
routing fields. Runtime routing derives one lifetime claim and socket from the
exact immutable session id. PIDs, process generations, and a global catalog are
not authority. An absent or safely unlocked claim is not running; a contended
claim must admit the exact expected session or resolution fails incomplete.
Only the claim owner may reclaim or retire the deterministic socket. Bounded
listing scans claims, but targeted routing never scans unrelated daemons.
The binding carries a tagged exact-agent or bare-entrypoint recipient. Bare
selection and exact-agent inventory validation happen only after callback
authentication and target-policy/session revalidation; exact known-address
behavior remains independent of entrypoint advertisement. Runtime lookup,
connection, callback, and send work is bounded by shared absolute deadlines and
non-queued process/connection admission. Disconnect or daemon shutdown cancels
the associated work, and stale-generation completions cannot publish projections.
Callback correlation precedes peer input admission and any auto-start creation.
The target event loop owns bounded live single-flight selection, and revalidates
entrypoint role/provider/skill authority immediately before receive commit.
An authenticated peer entrypoint keeps peer provenance and treats the message as
untrusted request content, but receives its locally configured role- and
policy-filtered ordinary tool surface. This lets a cooperative same-UID peer
handover perform verification and reporting without a second target-session UI
prompt; peer payload cannot expand the target's configured tool authority.
Best-effort at-least-once delivery deliberately has no distributed crash
transaction; an ambiguous retry may duplicate a receive occurrence, activation,
agent, model work, or spend.
Each accepted directional occurrence nevertheless remains its owning
transcript's sole canonical payload authority. Ordinary outbound `Message`
occurrences have no sender provider rendering; applicable inbound projections
remain provider context. Live recipient activation is a runtime-only,
sequence-keyed wake; replay reassembles the applicable provider context without
waking. Complete placement, rendering, checkpoint, and branch behavior is specified by
[SPEC-agent-message-delivery](SPEC-agent-message-delivery.md).

Peer-session discovery uses a metadata-schema-versioned `peer_entrypoint` hint
only to select bounded probe candidates. A live target RPC confirms the active
session and effective receiver capability before a session is returned.
Discovery never exposes socket paths, pids, full project roots, agent ids,
prompts, tasks, models, tools, or provider state.

New or reframed free-form payloads in the shared generic user-role text carrier follow [SPEC-exact-sentinel-prompt-envelopes](../specs/SPEC-exact-sentinel-prompt-envelopes.md).
