# Design decisions

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
provider namespace; missing credentials or unknown models finish with a provider
error rather than leaving the harness waiting.

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
request replay catch-up. Worker-produced provider progress/result frames return
to the main provider loop as typed protocol messages and are serialized through
the normal tau-client writer path.

Cancellation state is shared between the event loop and workers. Targeted
cancels remove queued prompts immediately and abort retry sleeps for matching
active prompts; disconnect/shutdown aborts all retry sleeps. Network reads are
still owned by the backend transports, so cancellation is guaranteed at retry
boundaries and queue boundaries rather than as a hard interruption of an
in-flight upstream read.

## Provider retry sleeps are capped per attempt

Status: unconfirmed

Transient ChatGPT/Codex errors use this crate's retry loop. Main-agent turns get
the normal retry count and extension-originated side turns get a smaller retry
cap. Each individual sleep is capped by `LLM_MAX_RETRY_DELAY` so provider
`Retry-After` and account-reset metadata cannot monopolize a prompt worker for
hours.

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
