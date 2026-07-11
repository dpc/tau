# Design decisions

Provider adapters classify canonical context-window rejection into the typed
terminal failure category. They do not authorize reactive recovery:
`ContextRecoveryDisposition` is harness-owned and any provider-supplied value is
discarded at ingress before eligibility is evaluated.

This file records major design decisions currently embodied by this directory's
code, and how authoritative each decision is. It is not an architecture
overview, ADR log, todo list, roadmap, implementation guide, or changelog.

## Provider profiles own built-in provider namespaces

Status: inferred

`tau-ext-provider-builtin` treats Tau state `auth.d/<provider>.json` files as the
source of truth for built-in provider registration. The filename supplies the
provider namespace, while the serialized profile `kind` selects the backend
family.

Model publication follows profile ownership: `chatgpt` profiles publish the
ChatGPT/Codex model matrix owned by `tau-provider-chatgpt`; `chat_completions`
profiles publish their configured model list; and `openrouter` profiles convert
to Chat Completions configuration and publish their configured or fetched model
list. Prompt dispatch resolves exact configured model IDs for the selected
provider namespace. Missing or invalid mutable profile/model/auth state remains
visibly pending and is re-resolved before later attempts.

## Prompt execution uses bounded workers with cooperative cancellation

Status: inferred

The runtime loop reads harness events, resolves prompt backends, and runs prompt
jobs on a bounded worker pool. `TAU_BUILTIN_PROVIDER_PROMPT_CONCURRENCY`
overrides the default concurrency of four workers.

Protocol startup and harness input dispatch run through `tau-client`'s manual
loop runtime. Startup publishes `ClientKind::Provider`, exact subscriptions for
prewarm/session-dir/cancel events, the current `ProviderModelsUpdated` snapshot,
and then `Ready`. Direct `agent.prompt.created` deliveries are handled as routed
live provider events without adding a subscription, so prompt execution does not
request replay catch-up. Worker-produced provider result/update frames return to
the main provider loop as typed protocol messages and are serialized through the
normal tau-client writer path. The worker side channel is event-driven:
each worker message must be enqueued before calling `ManualRuntimeWaker::wake`.
Wakes are coalesced and payload-free, so the main loop must drain harness input
and worker messages until both are empty before blocking in
`ManualExtensionRuntime::wait_for_wake`.

Cancellation state is shared between the event loop and workers. Targeted
cancels remove queued prompts immediately, abort retry sleeps for matching active
prompts, and notify any backend transport that registered a per-prompt abort
waker. Disconnect/shutdown aborts retry sleeps and wakes all registered backend
abort wakers. Backend network reads remain transport-owned and cancellation is
cooperative rather than a hard socket interruption; ChatGPT/Codex WebSocket turns
use the abort-waker path to wake an idle provider-event receive and return the
normal harness cancellation result promptly.

Built-in providers batch `provider.response_updated` output. They may emit the
first non-empty streamed output sample promptly, then emit later non-terminal
progress at most once per second per prompt. Worker/backend stream loops must not
write Tau protocol progress events directly from every upstream chunk; transport bytes, visible deltas, compaction status, and public content-free
response stats are accumulated until the rate-limited emitter samples them,
except for the first non-empty sample and for a terminal flush immediately before
the provider prompt closes.

## Required provider work retries outside the worker pool

Status: confirmed by approved product policy for `tau-agent-jbkk`

A logical prompt remains pending across retryable provider attempts until it
succeeds, is canceled, the process/session shuts down, or the unchanged request
is positively proven deterministic and invalid. Unknown remote failures retry;
classification selects cadence, shared cooldown, visible explanation, and
profile reload behavior rather than default termination.
Provider adapters attach a machine-readable terminal failure category to the
single final `ProviderResponseFinished`. Terminal request rejections bypass the
logical-work retry scheduler even when its configured retry budget is
effectively unlimited.

Workers execute one finite attempt. Retryable outcomes return the logical job to
one process-lifetime delayed scheduler, releasing the bounded execution slot
before any wait. Jittered Fibonacci cadence reaches about one minute for
transport/overload/throttle and at most thirty minutes for persistent usage,
account, auth, and unknown failures. Trusted `Retry-After` or structured reset
hints are lower bounds and may be later than that generated ceiling. Prompts
using one provider profile share limit cooldowns, while cancellation remains
prompt-scoped. Mutable profiles and credentials are reloaded when delayed work
becomes due.

Retry state is memory-only. Cold restart intentionally does not replay an
ambiguously accepted request because doing so can duplicate output, cost, tools,
or side effects.

## Provider diagnostics require an existing durable session directory

Status: unconfirmed

Provider debug request/response captures may include full prompt text, tool
results, and model output. The extension derives an explicit diagnostics policy
from `harness.session_dir` current-state events and passes that policy into
backend debug writers. Shared backend helpers only return debug paths when that
explicit durable-session signal is true and the durable session directory already
exists; provider diagnostics must not infer durability from filesystem shape or
create per-session roots on their own. This preserves ephemeral-session
persistence boundaries even when an ephemeral run reuses a session id with older
durable state.

## This crate tests registry/runtime integration, not backend protocol matrices

Status: inferred

This crate's tests cover provider profile serialization, CLI behavior, model
publication/routing, runtime event ordering, cancellation/retry bookkeeping, and
final provider event shapes. Backend wire-format parsing and HTTP/SSE/WebSocket
transport details belong in `tau-provider-chatgpt` and
`tau-provider-chat-completions`; this crate should test its integration with
those backends without duplicating their protocol parser matrices.

## Structured provider retry facts

Status: confirmed, 2026-07-11, dpc

Provider retries carry closed structured categories, saturating attempt counts,
and approximate bounded delays independently of human UI prose. Providers emit
only these safe facts alongside their local display status; the harness
validates prompt ownership and exclusively owns watcher snapshots and fanout.
