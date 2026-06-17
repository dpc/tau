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

Extensions that need to turn external user input into a normal agent prompt use
`extension.prompt_submit_request`. The harness accepts this request only on the
extension path, validates the target loaded agent, and then submits a normal
user prompt through the same machinery as UI prompt intake. The durable
transcript fact remains the harness-owned `agent.prompt_submitted`; extensions
may not forge prompt or message transcript facts directly.

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

## Skills

The harness owns canonical discovered-skill state. Extensions such as `tau-ext-shell` announce candidate skill files, but the harness validates names/descriptions, resolves collisions by selected winner, stores user/model invocation flags, and builds model-visible prompt/tool snapshots from the current winners. `disable-model-invocation` removes a winner from `<available_skills>` and from the internal `skill` tool snapshot; it is a prompt-surface policy, not a filesystem security boundary.

User `/skill <name> [args]` and `/skill:<name> [args]` expansion is performed at harness prompt intake for both existing-agent prompts and new-agent initial prompts. Unknown, invalid, unreadable, or non-user-invocable commands emit `harness.notice` and are not submitted as model prompts. Successful invocations read a bounded skill-file prefix, strip frontmatter, and store the expanded Pi-style `<skill>` block in the normal prompt transcript.

## Lifecycle events

Harness lifecycle events such as session start/shutdown and extension status are
normal events unless specifically marked must-pass/immutable. Session lifecycle
facts are protected because extensions and context providers use them to set up
or tear down per-session state. Extension lifecycle/status events are runtime
observability facts and may be intercepted like other non-protected events unless
call-site policy says otherwise.
