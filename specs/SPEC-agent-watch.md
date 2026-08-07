# SPEC-agent-watch: Agent watch

## Record justification

Agent watch behavior spans harness-owned topology and dedupe state, typed
protocol snapshots, provider-work observation, model-context rendering,
cross-agent activation, replay, and endpoint cleanup, so no one owning module
can describe the complete observation and lifecycle contract coherently.

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

Rejection publishes no topology snapshot or initial work/provider state event.
Re-enabling an existing edge retains its established behavior and subscription
identity. Disable bypasses cycle analysis. See
[GATE-agent-watch-acyclic-topology](GATE-agent-watch-acyclic-topology.md).
Clients may derive recursive UI activity over this live DAG, but that projection
does not create protocol state, lifecycle edges, or model-visible notifications.
Direct watch lifecycle facts remain scoped to their individual subscription edge.

## Model-visible notifications

`agent_watch` is a model-visible cross-agent content exposure boundary. A watcher may
receive hidden typed context projections containing the watched agent's final response
text or the text of a user prompt accepted by the watched agent. These notifications must be
clearly labeled as watch notifications, not as explicit `message` tool deliveries:

- `<tau_internal>Watched agent <agent-id> emitted a response …</tau_internal>`
- `<tau_internal>Watched agent <agent-id> received a user prompt …</tau_internal>`

Self-reported work status uses a closed phase (`unreported`, `working`, `done`,
`blocked`, or `unknown`), a runtime-local epoch, and a canonical model-authored
title of at most 160 UTF-8 bytes. The title is nonempty, trimmed, single-line,
and contains no control characters. An `unreported` notification carries no
title. Initial snapshots do not activate the watcher. Later status transitions
are durable typed, isolated notifications. Prompt presentation escapes the title
as untrusted visible metadata and uses the generic shape
`<tau_internal>Watched agent <agent-id> status: <state> on <title></tau_internal>` without
inferring start/update sequencing.

The default `status` tool remains subject to each effective prompt's ordinary
tool policy. Each dispatched visible-user or external-message activation and
each ordinary agent-message activation (direct message, watched final response,
or watched user prompt) containing `status` receives one acknowledgement
opportunity. An accepted status report or delivered notice
suppresses reminders across later routine tool-round snapshots. Isolated
watch-notification turns never request acknowledgement from the watcher. Done
and Blocked mutate reported status only, never close a turn or install a wait,
and a later activation does not silently change them.
Tool guidance asks agents to report meaningful user-level work rather than
routine progress or label-only changes, and to batch status with independent tool
calls when possible. The model-visible status argument names this label
`task_name` and asks for an independently informative label rather than an
opaque identifier or task/ticket number alone; internal events and projections
continue to call the canonical metadata `title`.
Rejected `status` calls emit and persist their human-readable diagnostic with
no `ToolError.details`; rejected state and `task_name` fields must not resemble an
accepted or current status. This event-payload semantic was explicitly approved
under
[GATE-persistence-and-extension-interface-change-approval](GATE-persistence-and-extension-interface-change-approval.md).

Successful no-tool finals while Working are durable candidate responses. Watch,
delegated result, and detach projections remain withheld until the candidate's
semantic append commits. A bounded challenge continues the same outer turn;
exhaustion accepts the committed candidate and changes Working to Unknown.
Unsuccessful terminals bypass challenges and change Working to Unknown once.
Current status is runtime-only; durable typed message facts provide projection
and replay authority. A newly enabled watcher receives only the current snapshot,
never historical transitions or thresholds.

Installed waits accumulate the union of monotonic elapsed time within the current
Working epoch. A wait resolved or rejected before installation contributes no
duration. Every terminal path after installation, including ordinary settlement,
interruption, and timeout, contributes its actual installed duration. A settled
wait pauses accounting, and a later installed wait resumes from the accumulated
duration. Starting a new Working epoch resets both accumulated duration and
crossed thresholds.

The harness schedules event-loop deadlines at 15, 30, 60, 120, 240, 360, and
subsequent 120-minute thresholds. Each crossing emits one durable typed,
isolated notification to every current watcher without waking the waiting agent.
Accounting and threshold cursors advance even when there are no watchers. A
watch enabled later receives the current work-status snapshot but no historical
long-wait notification. Unload and session rollover discard runtime accounting
and deadlines; replay retains committed recipient projections as context without
reconstructing timers or re-fanning notifications.

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
model-visible provider and work-status transitions use
isolated activation. Initial and
redundant structured snapshots have no wake and no provider block. Replay
retains canonical facts without waking or refanout.
