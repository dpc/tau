# tau-harness architecture

`tau-harness` owns the daemon-side control plane for Tau sessions. It connects
clients and extensions, sequences events, applies interception, persists durable
session/agent facts, and delivers committed events to subscribers.

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

Extensions that need to turn external user input into a normal agent prompt use
`extension.prompt_submit_request`. The harness accepts this request only on the
extension path, validates the target loaded agent, and then submits a normal
user prompt through the same machinery as UI prompt intake. The durable
transcript fact remains the harness-owned `agent.prompt_submitted`; extensions
may not forge prompt or message transcript facts directly.

Cross-harness agent messages use the dedicated `ExternalAgentMessage` protocol
RPC, not `Emit`. The sender-side built-in `message` tool parses
`<session-id>/<agent_id>` addresses, treats the current session as local, and
performs runtime-dir lookup plus socket round-trip on a helper thread. Completion
returns to the event loop as a `HarnessCommand`, so target socket latency never
blocks normal event processing. The receiver accepts the RPC only from a socket
peer that completed the narrow external-message hello, validates that the target
session is its active `current_session_id` and the recipient is live or pending,
then publishes the harness-owned inbound `agent.message_received` projection
through the ordinary durable path. Generic peer-authored
`agent.message_sent`/`agent.message_received` emits remain rejected.
Runtime-dir discovery verifies matching candidates by connecting to their
sockets. A failed probe is not enough to unlink discovery files while the
metadata pid is still live; dead-pid entries are eligible for cleanup on
platforms where Tau has a safe pid-liveness backend, so a transient probe
failure does not permanently hide a running daemon.

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

## Lifecycle events

Harness lifecycle events such as session start/shutdown and extension status are
normal events unless specifically marked must-pass/immutable. Session lifecycle
facts are protected because extensions and context providers use them to set up
or tear down per-session state. Extension lifecycle/status events are runtime
observability facts and may be intercepted like other non-protected events unless
call-site policy says otherwise.

## Provider response update routing

The harness treats `provider.response_updated` as non-durable live progress. It validates that the publishing connection owns the in-flight provider prompt, overwrites the update `agent_id` from harness prompt ownership, enriches best-effort compaction metadata, and does not include these transient deltas in durable replay.

## Prompt dispatch lifecycle split

Prompt dispatch emits a lightweight transient `agent.prompt_started` lifecycle
fact immediately before the full `agent.prompt_created` provider work request.
Providers consume `agent.prompt_created`; UIs and side-effect observers should
subscribe to `agent.prompt_started` so materialized prompt context and tool
schemas are not sent over UI/control channels unnecessarily.
