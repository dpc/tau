# tau-harness security notes

## Daemon IPC listener lifecycle

The harness daemon listener is local IPC for trusted same-user Tau clients and
runtime discovery. Listener ownership and cleanup must preserve the socket
identity checks in `tau-socket`; a daemon-owned listener should outlive cloned raw
listener fds used by accept-forwarder threads, and socket-activated listeners must
not be unlinked by the harness.

Accept-loop shutdown must use an owned wake/cancellation primitive tied to the
accept thread, not polling sleeps and not the filesystem socket pathname. Runtime
socket paths can be removed or replaced while a cloned listener fd remains live, so
shutdown correctness must not depend on reconnecting to that path. Internal wake
traffic is control-plane state only and must never be forwarded as a harness
client.

## Provider response stats

Provider response stats are public, content-free metadata on transient `provider.response_updated` events. They may carry routing ids, originator, cumulative backend response byte counts, and elapsed provider request time, but must not include provider text snippets, raw wire payloads, tool names, raw arguments, tool results, prompt text, or backend diagnostics.

The harness validates provider prompt ownership and rewrites the public `agent_id` from prompt ownership before broadcasting provider updates. It must not strip `response_stats`, derive its own response-throughput counters, or emit a harness-owned response-throughput projection. Stats samples are transient and must not be folded into agent transcripts, editor state, prompt stdin output, or final assistant rendering.

## Agent watch notification boundary

`agent_watch` is a model-visible cross-agent content exposure boundary. A watcher
may receive hidden internal prompts containing the watched agent's final response
text or the text of a user prompt accepted by the watched agent. These
notifications must be clearly labeled as watch notifications, not as explicit
`message` tool deliveries:

- `[tau-internal]: Watched agent <agent-id> emitted a response`
- `[tau-internal]: Watched agent <agent-id> received a user prompt`

Content-free outer agent-turn initial/start/stop notifications are also allowed.
An agent turn covers the complete activating-input-to-terminal-response
lifecycle, including inner model and tool rounds. These notifications
contain only stable watch/session identity, idle/running state, snapshot status,
and a harness-runtime-scoped watched-agent turn generation. They must never include prompt,
response, message, tool, or error content.
Initial snapshots are not model input. Later model-visible transition wording
is reconstructed from the typed state and watched identity, never trusted from
the event's compatibility message field.

Unloading either endpoint retires the complete watch relation and its current
provider/delivery state before later fanout. Fanout additionally requires both
endpoints to remain live, preventing durable recipient records from accumulating
when live prompt delivery is impossible and preventing stale provider status
from crossing a same-session reload.

Activating-input waits are scheduling, not a new input authority. They wake only
after canonical accepted input is committed or queued for that exact target
agent, using harness-owned inference-activation classification. Raw extension or
transport traffic, rejected or replay-only envelopes, asserted sender labels,
and another agent's input cannot wake them. The wait result is bounded and
content-free (`input_available: true`): external payload, sender, provenance, and
reply capability remain solely in the canonical typed envelope delivered by
normal prompt machinery and are never rewrapped as harness-authored tool output.
Wait registration is runtime-only harness state: cancellation, target unload,
session rollover, and shutdown remove it, and cold recovery uses ordinary
unresolved-tool repair rather than reviving stale scheduling authority.

Model-visible watch enable classifies the target and mutates watch state in one
harness-loop operation. Only a Live target is accepted. Stopped or Unknown
rejection leaves forward and reverse topology, subscription identity, provider
snapshot, and delivery-dedupe state unchanged; reloading the same id requires a
fresh enable and subscription.

A turn caused only by lifecycle notifications suppresses both lifecycle edges
to prevent cyclic watches from self-exciting. If ordinary input joins that
generation during a tool/provider continuation, the harness first emits the
delayed start edge, then emits the matching stop normally; it never exposes an
orphan stop.

The watch path must not forward internal steering prompts, background or
foreground tool-completion prompts, explicit `message` tool deliveries to the
watched agent, or other hidden/non-user inputs. A completed `agent_start` result
is the started child agent's terminal final response to its direct delegating
watcher and remains watchable under the response label.

Provider-authored status text, response bodies, headers, prompt text, account identifiers, and raw errors never cross the watch boundary. Only protocol closed enums and bounded numeric retry facts are accepted after prompt ownership validation. Terminal watched responses use the typed failure kind rather than `ProviderResponseFinished.error`.
Per-subscription delivery dedupe retains at most 64 nonterminal prompt identities
for the newest observed turn generation and rejects older generations without
mutation. Terminal-error delivery retires its prompt immediately. Capacity
evicts the oldest nonterminal prompt, so a later status for that evicted prompt
is intentionally treated as first delivery again; same-category suppression is
guaranteed only while the prompt remains retained. Cardinality instrumentation
contains only subscription and generation identity, counts, and closed
delivery/eviction decisions, never prompt ids or provider-authored content.

Reactive context recovery never trusts provider prose or a provider-authored recovery decision. Eligibility uses the closed failure category, an empty output set, harness-owned prompt operation/model routing, durable activation cut, advertised model capability, and role policy. Watchers receive only the existing sanitized `recovering_context` state; prompt bodies and raw provider errors are not included.

Provider-supplied recovery disposition is unconditionally cleared at ingress and may only be stamped by the harness after eligibility checks. Any accepted streamed semantic output makes the response recovery-ineligible. Cancellation durably terminalizes an active reactive compaction transaction.
## Compaction tools

Internal registration places `compact` in `compaction` and `agent_compact` in
`cross_agent_compaction`; both are disabled by default. Runtime caller identity
comes from committed tool ownership, never arguments. Cross-agent possession
authorizes any other loaded agent, but self, unavailable, stopped, unloaded,
and cross-session targets are rejected without state enumeration. Watches,
messages, ancestry, and automatic-compaction role settings are not substitutes
for explicit tool presence.
## Context-limit telemetry

The harness, not providers, owns the durable context-limit diagnostic attached
to terminal responses. Provider-supplied values are discarded. The schema is
content-free and bounded to one record per rejected prompt: model id, operation,
optional token counts/window, optional exact serialized transcript-growth bytes,
reserve, active threshold, closed policy/eligibility/action, and a closed
observation enum. Exact growth and its projection are absent if a supported
raw-CBOR transcript entry cannot be represented as JSON or the checked total
overflows. A categorical observation requires a positive advertised limit and
nonzero provider input usage; the byte-derived projection only corroborates or
makes contradictory evidence insufficient. Raw evidence remains present even
when the bounded observation is insufficient. Raw prompts, errors, response
bodies, headers, accounts, and endpoints are excluded.
Normal session/event retention applies; watcher snapshots do not duplicate this
record. Evidence never automatically lowers limits or thresholds.
