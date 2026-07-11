# tau-harness architecture

## Canonical transport message boundary

Extensions register a transport family, send tool, and zero or more exact
proactive alias capabilities. Registration is bound to the authenticated
connection and current session generation; refresh replaces the route set, while
tool unregister, disconnect, and session rollover revoke it. Dedicated
ingress/send-completion RPCs replace generic event emission; the harness stamps
instance, agent endpoint, trust class, canonical id, and commit time, then owns
the protected durable fact. Per-peer retention is capped at 16 distinct transport
capabilities, in addition to the route-count and encoded-metadata bounds on each
registration.

Deduplication precedes publication and a bounded index is lazily rebuilt from
typed transcript entries. Source sequence checks are scoped by extension,
transport, conversation, and thread; durable append order remains authoritative.
Only the live post-commit hook acknowledges ingress and activates the runtime
route. It queues message identity, durable sequence, and an optional transcript
node; tool-adjacent messages remain unresolved until tool closure. Replay
reconstructs unacknowledged typed wakes from their durable identities and
inference watermarks rather than replaying live acknowledgements.

Remote transport acceptance cannot be transactional with Tau storage.
Successful-send completion validates the live call and either opaque reply route
or exact alias/endpoint/native conversation/fixed-thread capability, then queues
the outgoing fact immediately before its terminal tool result and caches exact
retry results. A crash can still leave remote and local state different; the
recorded acceptance must not imply delivery or read receipt.
The durable outgoing fact records authorization and tool-call audit before the
terminal result. Capability input is bounded and duplicate native
conversation/thread routes are rejected independently of presentation metadata.

`tau-harness` owns the daemon-side control plane for Tau sessions. It connects
clients and extensions, sequences events, applies interception, persists durable
session/agent facts, and delivers committed events to subscribers.

## Daemon listener and accept forwarding

Daemon IPC sockets are bound or socket-activated before the harness event loop
starts. A small accept-forwarder thread converts accepted Unix streams into
`HarnessEvent::NewClient` messages for the event loop; all client protocol
validation still happens after the stream reaches the harness.

The forwarder waits reactively on the listener fd plus an owned wake fd. Dropping
the forwarder wakes and joins the thread before the daemon listener handle is
dropped, preserving socket cleanup ownership while avoiding sleep polling and
path-based shutdown races.

## Event sequencing, interception, and persistence

All ordinary event publication should flow through the central publish path:
`enqueue_publish` runs interceptors in priority order, `commit_event` stamps a
single runtime sequence/timestamp, writes debug/event-log records, persists
eligible semantic facts, and broadcasts delivery frames. Direct calls to
`commit_event` are reserved for code that has already resolved interception.

Interceptors are local privileged extensions. They can inspect, modify, or drop
most matching events before commit. The harness protects selected facts as
must-pass and immutable because live state, durable resume state, and transcript
routing must agree. Fully immutable facts include session lifecycle facts,
session membership facts, `agent.started`, harness-owned agent message
projections, terminal tool completion facts (`tool.result`, `tool.error`,
`provider.tool_result`, `provider.tool_error`, `tool.cancelled`,
`tool.background_result`, and `tool.background_error`), and selected response
closure facts such as `provider.response_finished`. Prompt text facts are
must-pass, but only their routing keys are immutable: interceptors may rewrite
text on the sanctioned prompt-text events without changing agent id, message
class, or originator. Mandatory `harness.notice` diagnostics (critical notices
and `always_show` warnings such as extension config errors) are replayable,
published with a call-site `must_pass` override, and protected from interceptor
rewrite/drop.

`tool.request` and `tool.started` are session-scoped execution restore facts.
They are persisted in each session's `restore-events.cbor` stream (or the
equivalent in-memory stream for ephemeral sessions), replayed only to peers that
request matching `historical_selectors`, and deliberately kept out of agent
transcript logs. Live tool execution remains driven only by non-replay
`tool.started` deliveries.

Catch-up snapshots reconstructed from current harness state (for example
`session.agent_loaded`, folded metadata, and `harness.session_dir`) are also
selected by `historical_selectors` and delivered with `EventDelivery.replay =
true`. Only `agent.replay_complete` and `session.replay_complete` boundaries
remain non-replay during catch-up; live delivery is buffered until the session
boundary has been sent.

## Agent display names and watch topology

Agent display names are human-facing labels, not topology metadata. They may
come from a user-supplied topic, role/template rendering, or an explicit rename,
but they must not encode parent/child lineage or watcher relationships.
Topology and observation state belong in protocol facts instead, so the same
agent label remains stable wherever the agent is referenced.

Session-local watch state is represented by authoritative
`agent.watches_updated` snapshots keyed by watcher. The harness maintains the
forward watch set and reverse watcher index only as runtime/session state,
publishes complete replacement snapshots for each watcher, and does not persist
watch relationships into agent display names.

Model-visible `agent_watch` notifications are deliberately narrow. A watcher may
receive only a watched agent's final response notification (`Watched agent <id>
emitted a response`) or a watched agent's received user-prompt notification
(`Watched agent <id> received a user prompt`). Internal steering, background
tool-completion prompts, explicit `message` deliveries to the watched agent, and
other hidden inputs must not be forwarded as watch notifications. A terminal
`agent_start` result remains the started agent's final response to its direct
delegating watcher, even when that watcher is itself a side agent.

In addition to those two content-bearing kinds, watches receive content-free
whole-model-turn state notifications. The canonical `AgentTurnState` mapping
emits one initial snapshot and subsequent idle/running edges; tool continuation
rounds remain one generation. Lifecycle-only notification turns do not fan out
more lifecycle notifications, preventing mutual-watch feedback loops.
The initial snapshot remains durable client-visible state but is not injected
into the watching model's context. Later transition prompts are derived from
the structured payload rather than treating their compatibility text as an
agent-authored message.

## Provider response stats boundary

Providers own response-throughput sampling. A provider starts prompt-local response stats when it dispatches the backend request, counts backend response bytes at the transport receive boundary before semantic parsing, batches counters in memory, emits the first non-empty previous/current `response_stats` sample promptly, emits later samples at most once per second on `provider.response_updated`, and may flush once immediately before the prompt closes. `previous` is the last sample that provider actually emitted, not an internal calculation.

The harness is not part of response-throughput accounting. For `provider.response_updated`, it validates provider prompt ownership/cancellation, rewrites `agent_id` from harness prompt ownership, enriches provider compaction metadata when applicable, and broadcasts public provider updates, including stats-only updates. It must not strip provider `response_stats`, map them to agent-turn stats, maintain response byte counters, or schedule idle response-stats samples. UI clients subscribe to provider updates and render provider-owned stats directly.

## Session and agent stores

The session store owns durable membership facts such as
`session.agent_loaded` and `session.agent_unloaded`. `session.started` and
`session.shutdown` are must-pass, immutable runtime/current-session snapshot
facts, but they are not folded into the durable session membership store. Agent
stores own durable transcript facts, including `agent.started`, prompt facts,
provider/tool results, harness-owned inter-agent message projections, and
per-agent metadata set/unset facts. Metadata is committed through the same
interceptable publish path as other ordinary events; the folded latest metadata
snapshot is replayed to subscribers before `session.agent_loaded`, and
inheritable entries are copied to child agents when an explicit or derived
parent is known. Tests should assert durable stores, not only runtime delivery,
when changing durable facts.

Loading an existing durable agent into a session that has not previously
contained it queues a one-shot hidden notice for that agent's next user prompt.
The notice warns that session-scoped tool and extension state can differ and
calls out timers as setup that may need recreation. Cold session resume uses the
same guidance in its restore notice because runtime can stop before the
agent-specific notice is folded into a durable prompt. The harness caches
ever-loaded membership for the bound session, including memory-only ephemeral
membership, so repeated prompt routing does not rescan journals and same-session
unload/reload does not produce a false warning.

Ephemeral session mode (`tau --ephemeral`) replaces the session membership store
with an in-memory store for the current harness process and suppresses
session-owned disk artifacts: membership logs, metadata/locks, debug
`events.jsonl`, per-session stderr logs, and session-scoped extension data.
This does not make agents ephemeral: the global agent store remains durable, so
prompts, responses, tool results, metadata, and sub-agent transcripts keep their
normal persistence. User/cache extension data, provider state, credentials,
policy/config files, and runtime sockets are also outside the session-ephemeral
boundary.

Agents can separately be staged as ephemeral from the TUI (`/new` then
`/ephemeral on`). That policy is per agent: the harness marks the agent id before
the first semantic write, stores its transcript and metadata in the live
`AgentStore` only, and folds its `session.agent_loaded` membership fact in memory
without appending it to a durable session journal. Late subscribers attached to
the same daemon replay those memory records, but cold resume sees only durable
agents. Children of ephemeral parents inherit the memory-only policy so delegated
work does not accidentally create durable child transcripts.

The debug JSONL mirror is part of this boundary: content-bearing agent, prompt,
provider, tool, shell, or delegation events for ephemeral agents must be
classified before logging. New event kinds that carry agent transcript content or
reference prompt/tool-call ids must update that classifier and its regression
tests.

The debug JSONL mirror also has a narrow temporary redaction exception for
`action.invoke` events with action id `email.auth.google.finish`: the harness
redacts raw action arguments because the current action schema cannot mark the
pasted Gmail loopback URL as sensitive and that URL contains a one-time OAuth
authorization code. Routed `ActionInvoke` delivery still carries the raw
argument to the owning extension. Future schema/protocol sensitive-argument
metadata should replace this action-id-specific debug-log redaction.

## Client event boundary

UI clients are local UI/control peers, not providers. Client `emit` intake must
preserve provider ownership by routing provider-category events through the
extension/provider event path, where provider-source and prompt-owner validation
still apply. Non-provider client events are partitioned into harness-owned UI
commands, validated per-agent metadata set/unset facts, and a narrow fallback
allowlist. UI command handlers keep their existing keep-going result at the
dispatch-helper boundary; the outer client-message layer remains responsible for
connection lifetime. Metadata writes are validated and enqueued through the normal
publish path; fallback publication is limited to explicitly allowed UI/live
events and extension-owned custom events, using the explicit transient override
or the event default. Tool lifecycle/terminal facts and harness-owned lifecycle,
membership, transcript, and status facts must not be accepted through client
fallback.

UI debug/status commands that inspect local transport counters are direct live
responses to the requesting UI, not ordinary publish/replay traffic. Extension
protocol-I/O stats are exposed only through the `ui.debug_event_stats_request`
control path, answered with a directed non-persisted notice, and must not add
subscriptions or synthetic replay events.

## Extension boundary

Extensions are less-trusted peers connected over the Tau protocol. They may
publish ordinary events through `emit`, subscribe to committed events, register
interceptors, provide tools/actions/context, and request extension-data file
operations. The harness validates source ownership for harness-owned or
provider-owned facts and rejects peer-authored lifecycle, membership,
transcript, prompt, and harness-status facts unless they arrive through the
specific API path that owns them. Interceptor replacement is intentionally
conservative: protected facts may be observed, but drops and forbidden rewrites
publish the original event so routing identities and durable folds stay aligned.
Mutable prompt-text events may be rewritten only without changing their routing
identity.

The harness also tracks loaded session membership in runtime state before the
corresponding must-pass `session.agent_loaded` publish commits. That keeps
idempotency stable while an interceptor parks publication and prevents duplicate
membership/start facts from being queued for the same live agent.

Provider tool calls are evaluated against the tool snapshot owned by the prompt
that produced them. Model-visible rejection diagnostics for those calls must use
that same snapshot for availability wording and near-name suggestions; current
role/model policy is only the authority when no prompt-owned snapshot exists.
Tool examples are registration metadata, not prompt-surface definitions: rendered
tool definitions omit them, and the harness surfaces at most one bounded relevant
example after a failed call in an agent branch.

Extensions that need to turn external input or internal wakeups into an agent
prompt use `extension.prompt_submit_request`. The harness accepts this request
only on the extension path, validates the target loaded agent, and then submits
a user-style or hidden internal prompt through the same machinery as UI prompt
intake. Internal extension prompts do not update user-interaction metadata, but
still wake queued agents. The durable transcript fact remains the harness-owned
`agent.prompt_submitted`; extensions may not forge prompt or message transcript
facts directly.

Cross-harness agent messages use the dedicated `ExternalAgentMessage` protocol
RPC, not `Emit`. The sender-side built-in `message` tool parses
`<session-id>/<agent_id>` addresses, treats the current session as local, mints a
per-message bearer capability bound to sender identity, recipient, message body,
and message/watch-response kind, and performs runtime-dir lookup plus socket
round-trip on a helper thread. Completion returns to the event loop as a
`HarnessCommand`, so target socket latency never blocks normal event processing.
The receiver accepts the RPC only from a socket peer that completed the narrow
external-message hello, validates that the target session is its active
`current_session_id` and the recipient is live or pending, then calls back to the
claimed sender harness from a helper thread to authenticate the capability and
bound fields. The helper completion returns to the event loop as a
`HarnessCommand`; only then does the receiver publish the harness-owned inbound
`agent.message_received` projection through the ordinary durable path. Generic peer-authored
`agent.message_sent`/`agent.message_received` emits remain rejected.
Runtime-dir discovery verifies matching candidates by connecting to their
sockets. A failed probe is not enough to unlink discovery files while the
metadata pid is still live; dead-pid entries are eligible for cleanup on
platforms where Tau has a safe pid-liveness backend, so a transient probe
failure does not permanently hide a running daemon.

## Harness-owned tool-call id scoping

The harness-owned `wait` and `cancel` tools treat explicit `tool_call_id`
arguments as scoped to the calling conversation. Exact `wait` requests must check
that the target call is owned by the waiting conversation before duplicate-wait,
queued-input preemption, or stored-result handling. `cancel` requests must check
that the target call is owned by the cancelling conversation before consulting
duplicate-cancel or completed-call state and before publishing
`tool.cancel_request`. Cross-owner probes use the same unknown-id behavior as
absent calls so tool-call existence, completion state, and already-cancelled
state do not leak across agents.

## Optional extension startup

Extension startup availability is controlled by resolved `ExtensionConfig.require`.
Required extensions preserve startup-fatal behavior for harness-owned init
failures such as missing commands, missing required declared secrets, spawn
failure, and pre-Ready timeout. Other pre-Ready disconnect handling follows the
existing compatibility behavior unless the disconnect is already provider/socket
fatal. Optional extensions (`require: false`) are skipped or disabled for
startup/config/secret/pre-Ready failures, but the failure must still be emitted as
a mandatory replayable `harness.notice` so initial and late UI subscribers see why
the extension is absent. This policy is limited to startup/init availability; do
not broaden it into new post-Ready respawn or runtime-failure semantics without a
separate design change.

## Extension data

Extension-data RPCs confine paths to per-extension state roots, reject traversal
and symlink escapes, write private files/directories where supported, and enforce
per-file/per-directory-list quotas. Quota failures are reported as
`quota_exceeded`. These limits bound individual harness operations, not aggregate
extension disk usage across many files.

In ephemeral session mode, `ExtensionDataScope::Session` is rejected before any
session data root is created. `User` and `Cache` scopes remain durable because
they are extension-owned non-session storage.

## Skills

The harness owns canonical discovered-skill state. Extensions such as `tau-ext-shell` announce candidate skill files, but the harness validates names/descriptions, resolves collisions by selected winner, stores user/model invocation flags, and builds model-visible prompt/tool snapshots from the current winners. `disable-model-invocation` removes a winner from `<available_skills>` and from the internal `skill` tool snapshot, and makes it user-invocable; it is a prompt-surface policy, not a filesystem security boundary.

User `/skill <name> [args]` and `/skill:<name> [args]` expansion is performed at harness prompt intake for both existing-agent prompts and new-agent initial prompts. Unknown, invalid, unreadable, or non-user-invocable commands emit `harness.notice` and are not submitted as model prompts. Successful invocations read a bounded skill-file prefix, strip frontmatter, and store the expanded Pi-style `<skill>` block in the normal prompt transcript.

Extensions that register with `extension.session_context_provider_register`,
subscribe to `session.started`, and publish session-wide prompt context such as
skills and AGENTS.md files must acknowledge completion with
`extension.session_context_ready`; eager startup waits for that acknowledgement
before considering startup discovery complete. Plain `session.started`
subscribers and per-agent-only context providers are not waited on unless they
explicitly register as session context providers. Role `required_skills`
validation runs after that startup/session skill discovery has completed. The
harness checks exact skill names against the selected model-visible skill
winners and verifies that the winning source is loadable with the same bounded
read/frontmatter rules as the `skill` tool. Roles with missing, hidden, or
unreadable required skills are removed from role selection/delegation and get a
mandatory replayable `harness.config_error` notice; if the selected/default
startup role is removed, startup fails rather than falling back silently.

## Tool prompt-surface policy

Extensions and providers publish metadata only: tools declare neutral `ToolTag`s
(such as `shell:edit:line`, `shell:edit:apply_patch`, `shell:exec:generic`,
`shell:exec:shell_command`, and `shell:cd`) and providers publish model
`ModelTag`s (such as `shell:chatgpt`). The harness owns all matching policy.

Tool enablement starts from each extension's `enabled_by_default`, then matching
harness `tool_policy.rules` run deterministically by `(priority, rule name)`,
with each rule applying `disable_tool_tags` before `enable_tool_tags`. Built-in
and user policy share the same evaluator; the built-in `builtin.chatgpt-shell`
rule disables `shell:*` for ChatGPT-tagged models and re-enables apply-patch,
shell-command, cd, and directory-lock tools.

Role precedence is broad-to-specific and runs after global policy: optional
`tools` allow-list base, `disable_tool_tags`, `enable_tool_tags`,
`disable_tool_groups`, `enable_tool_groups`, `disable_tools`, then
`enable_tools`. This deliberately lets a role disable a broad family and
re-enable a narrower tag, group, or named tool.

Prompt dispatch snapshots the effective `ToolSpec` list for the selected prompt
model. Provider tool calls are validated against that prompt-owned snapshot, not
against mutable current role/model state after the user switches roles or models
mid-turn. Staged tool registration can never expand a prompt snapshot after it
was sent.

Narrow schema-guided argument repair also uses the prompt-owned `ToolSpec`.
Repair runs only after pre-dispatch validation failure, applies a small fixed set
of mechanical conversions, revalidates before dispatch, and falls back to the
normal rejection diagnostics when repair is unsupported or still invalid. Repair
traces are bounded metadata for logs/UI, not prompt-surface examples.

The loop guard is runtime-only per loaded agent branch. It records compact recent
assistant/tool-failure signatures, injects one hidden pivot prompt for obvious
cycles, and surfaces a mandatory notice instead of continuing automatically if the
same cycle persists. New user prompts and successful tool results reset detector
history and remove pending loop-guard pivots, but preserve unresolved in-flight
tool-call argument signatures for sibling calls in the same turn. Branch/head
moves invalidate the whole guard, including in-flight signatures, and remove
pending loop-guard pivots.

Provider-side `repetition_detected` final responses feed this same lifecycle with
a fixed harness-authored reason: first occurrence queues the pivot, recurrence
after that pivot stops automatic continuation. Provider error text is displayed
but is not trusted as model-visible guard instruction.

## Durable branch-head navigation

The harness presents `/tree` as prompt rewind anchors by default. Numeric
anchors are one-based user-facing prompt positions; resolving an anchor moves
the durable branch head to that prompt node's parent, so the next user prompt
replaces or branches before the selected prompt. Root/before-first navigation is
represented by an explicit durable root head, while raw transcript node
navigation is only accepted through the explicit debug node target.
Default anchors are derived from durable prompt provenance, not merely from the
folded `UserInput` node shape: visible user-originated `agent.prompt_submitted`
facts and visible queued-user `agent.prompt_steered` facts are anchors, while
injected user messages, internal prompts, compaction triggers, assistant/tool
nodes, and agent-message projections are not.

`agent.head_moved` is durable cursor state, but it is not a permanent override
over later transcript records. Agent-log replay folds head moves and
node-producing events in chronological order; every later prompt/assistant/tool
node advances the folded `AgentTree::head()`. Resume therefore restores the
conversation cursor from the replayed tree head, preserving root head moves only
until a later branch-advancing event supersedes them.

Restored background-tool interruption notices are queued by session and owning
agent. They must be folded only into the next real user prompt for the agent
whose background call was repaired, so one loaded agent in a resumed session
cannot consume or see another agent's restored background-tool notice.

## Extension availability startup data flow

`tau-config` owns strict parsing of the supported names-only
`TAU_ENABLE_EXTENSIONS` input. The outer CLI parses and validates it early for
fresh-harness commands, preserving argv order for subsequent CLI operations.
Normal launches pass only ordered CLI operations through the private,
unstable `TAU_EXTENSION_CLI_OVERRIDES` child transport; the daemon command
clears inherited transport when there are no operations. The spawned harness
decodes that transport fail-closed. Direct in-process `component harness`
dispatch passes the same typed operations explicitly and does not consult the
private transport for them. Harness settings own the canonical final resolver:
config, public environment named enables, then ordered CLI overrides.

## Lifecycle events

Harness lifecycle events such as session start/shutdown and extension status are
normal events unless specifically marked must-pass/immutable. Session lifecycle
facts are protected because extensions and context providers use them to set up
or tear down per-session state. Extension lifecycle/status events are runtime
observability facts and may be intercepted like other non-protected events unless
call-site policy says otherwise.

## Provider response update routing

The harness treats `provider.response_updated` as non-durable live progress. It validates that the publishing connection owns the in-flight provider prompt, overwrites the update `agent_id` from harness prompt ownership, enriches best-effort compaction metadata, and broadcasts public updates. Displayable deltas, status, compaction, and content-free `response_stats` are all public provider-owned transient fields. Stats-only provider updates are valid and must be delivered to subscribers so UIs can render response liveness directly.

## Prompt dispatch lifecycle split

Prompt dispatch emits a lightweight transient `agent.prompt_started` lifecycle
fact immediately before the full `agent.prompt_created` provider work request.
Providers consume `agent.prompt_created`; UIs and side-effect observers should
subscribe to `agent.prompt_started` so materialized prompt context and tool
schemas are not sent over UI/control channels unnecessarily.

## Transient reply presentation

Durable envelopes retain source-owned `reply_path` for audit. Prompt assembly does not mutate that fact; it separately projects `reply` only when the route belongs to the target agent and the internally identified tool remains in the effective prompt snapshot, then uses its model-visible alias.
Standalone compaction is transaction-driven rather than inferred from a
transcript-tail trigger. A durable start captures an immutable branch cut plus
the pre-minted compact prompt id, provider-qualified model, and standalone
operation. The successful boundary repeats this harness-stamped tuple; core
accepts new boundaries only when all six transaction/cut/suffix/prompt/model/
operation fields are present, the transaction resolves its start,
cut/prompt/model/operation match it, operation is standalone, `suffix_end`
equals the boundary parent, and cut is its ancestor. Legacy boundaries have
all six absent. Runtime connection ids are deliberately not persisted:
they identify a daemon incarnation rather than durable provider work.
Only the start's post-commit reaction sends one cut-local compact request with
that exact prompt id and synthetic trigger. Success installs a
cut/suffix-bearing boundary so facts
committed during compaction survive after the replacement window. Terminal
failure records a safe durable category, blocks the owed activation from
automatic retry, and leaves the agent addressable for explicit recovery.
Inference resumes only after a durable dispatch watermark commits.
While that checkpoint is interceptable or waiting to persist, an explicit
`AwaitingCheckpoint` runtime state blocks every ordinary dispatch path. The
post-commit continuation sends the exact checkpointed prompt id and transcript
head, and acknowledges only materialized typed-message wakes on that branch
through the watermark. Replay folds transaction outcomes and inference
responses in core; an uncompleted checkpoint restores as dispatch-uncertain
rather than being silently duplicated.

Canonical submitted, injected, and steered transcript facts carry a
harness-owned `inference_activation` bit. Typed pending-prompt provenance—not
prompt text or peer input—decides the bit: active work is true, while passive
background and restore context is false. Interceptors may rewrite sanctioned
text but cannot change the bit. Missing legacy fields deserialize false.
Replay considers only true facts after the last completed checkpoint; an
uncompleted checkpoint remains uncertain and is never automatically resent.
