# SPEC-agent-watch: Agent watch

## Record justification

Agent watch behavior spans harness-owned topology and dedupe state, typed
protocol snapshots, provider-work observation, model-context rendering,
cross-agent activation, replay, and endpoint cleanup. No one owning module can
describe the complete observation and lifecycle contract coherently.

## Topology and endpoint lifecycle

Agent display names are human-facing labels, not topology metadata. For newly
created agents, the built-in default leaves agents without an explicit task or
rename unnamed, while operator-configured templates may still generate labels
from roles or other template context. Persisted names remain authoritative,
including older role-derived defaults that cannot be distinguished from
explicit values. Names must not encode parent/child lineage or watcher
relationships. Topology and observation state belong in protocol facts instead,
so the same agent label remains stable wherever the agent is referenced.

Session-local watch state is an acyclic directed graph represented by authoritative
`agent.watches_updated` snapshots keyed by watcher. The harness maintains the forward watch set and reverse
watcher index only as runtime/session state, publishes complete replacement snapshots
for each watcher, and does not persist watch relationships into agent display names.
Committed agent unload is an endpoint lifecycle boundary: it atomically retires every
incoming and outgoing relation, subscription identity, provider snapshot, and
delivery-dedupe bucket. Surviving watchers receive a replacement topology snapshot,
while no event is addressed to the unloaded endpoint. Watch enable classification and
mutation are one harness-loop operation. Only a Live target may create topology,
subscription, or notification state; Stopped and Unknown targets fail without changing
the forward or reverse topology, subscription identity, provider snapshot, or
delivery-dedupe state. Reloading the same id does not revive retired relations and
requires a fresh enable and subscription. Disable remains idempotent for known stopped
endpoints. After Live-target validation, a genuinely new `watcher -> watched` edge
is rejected without changing any watch state when `watched` already reaches
`watcher`, with diagnostic:

```text
agent watch would create a cycle: `<watcher>` -> `<watched>`
```

Rejection publishes no topology snapshot or initial turn/provider state event.
Re-enabling an existing edge retains its established behavior and subscription
identity. Disable bypasses cycle analysis. See
[DECISION-agent-watch-acyclic-topology](DECISION-agent-watch-acyclic-topology.md).
Clients may derive recursive UI activity over this live DAG, but that projection
does not create protocol state, lifecycle edges, or model-visible notifications.
Direct watch lifecycle facts remain scoped to their individual subscription edge.

## Model-visible notifications

`agent_watch` is a model-visible cross-agent content exposure boundary. A watcher may
receive hidden typed context projections containing the watched agent's final response
text or the text of a user prompt accepted by the watched agent. These notifications must be
clearly labeled as watch notifications, not as explicit `message` tool deliveries:

- `[tau-internal]: Watched agent <agent-id> emitted a response`
- `[tau-internal]: Watched agent <agent-id> received a user prompt`

Content-free outer agent-turn initial/start/stop notifications are also allowed.
An agent turn runs from activating input through the terminal response or
termination, including inner model and tool rounds. These notifications contain only stable
watch/session identity, idle/running state, snapshot status, and a
harness-runtime-scoped watched-agent turn generation. They must never include prompt,
response, message, tool, or error content. Initial snapshots are not model input. Later
model-visible transition wording is reconstructed from the typed state and watched
identity, never trusted from the event's compatibility message field.

A turn caused only by lifecycle notifications suppresses both lifecycle edges so
watch-derived activity cannot cascade along accepted acyclic chains. This is also
defense in depth for malformed topology. If ordinary input joins that generation during a
tool/provider continuation, the harness first emits the delayed start edge, then emits
the matching stop normally; it never exposes an orphan stop.

The watch path must not forward internal steering prompts, background or foreground
tool-completion prompts, explicit `message` tool deliveries to the watched agent, or
other hidden/non-user inputs. A completed `agent_start` result is the started child
agent's terminal final response to its direct delegating watcher and remains watchable
under the response label.

User-prompt watch fanout occurs only after the corresponding durable steer
commits. It uses the exact post-interception sanctioned steer text. A rejected
publication emits no WatchPrompt occurrence; eventual successful retry emits
exactly one occurrence after commit.

## Provider-work projection

Provider retries carry closed structured categories, saturating attempt counts, and
approximate bounded delays independently of human UI prose. After validating prompt
ownership, the harness owns the current per-agent/turn/prompt snapshot and session-local
watcher fanout. Live delivery is limited to first category, category/phase changes, and
terminal failure; same-category storms only refresh the late-watch snapshot while their
prompt remains among the 64 identities retained per subscription and generation.
Terminal-error delivery retires its prompt; capacity evicts the oldest nonterminal
prompt, which is treated as fresh if it reappears. Older generations cannot mutate newer
bookkeeping. Cardinality tracing contains only subscription/generation identity, counts,
and closed decisions. Enabling or re-enabling returns current sanitized state and emits
an initial client snapshot without prompting the model. Durable live facts replay as
transcript context without re-fanout; disable, prune, and session change stop delivery.
Raw provider bodies, status text, errors, headers, account data, secrets, and prompt
content never cross this boundary.

Every accepted watch-derived `AgentMessageReceived` is the sole canonical
payload projection for its occurrence and follows the sequence-aware placement
in [SPEC-agent-message-delivery](SPEC-agent-message-delivery.md).
`WatchResponse` and `WatchPrompt` use ordinary live activation. Noninitial
model-visible turn/provider transitions use isolated activation. Initial and
redundant structured snapshots have no wake and no provider block. Replay
retains canonical facts without waking or refanout.
