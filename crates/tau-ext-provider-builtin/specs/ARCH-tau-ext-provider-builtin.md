# ARCH-tau-ext-provider-builtin: tau-ext-provider-builtin architecture

`tau-ext-provider-builtin` is Tau's built-in provider bridge. It resolves local
provider profiles and credentials, publishes available models, receives
model-visible prompt and tool context from the harness, invokes external model
services, and reports provider execution through Tau protocol events.

## Ownership boundaries

Tau state `auth.d/<namespace>.json` files define provider namespaces, and each
profile's serialized kind selects its backend family. ChatGPT profiles use the
model matrix and finite inference facade owned by `tau-provider-codex`; Chat
Completions and OpenRouter profiles use the Chat Completions backend. The
extension owns mutable-profile resolution, model publication, event ordering,
public response sampling, logical retries, cancellation, and supervised prewarm
work. Backends return typed outcomes and never serialize harness frames.

ChatGPT profiles capture Responses mode at process startup. Model publication,
prompt, prewarm, retry, and quota resolution share that value; credential reload
and OAuth refresh do not change it. An on-disk mode edit takes effect after
restart, and different namespaces may select different modes. The selected
Codex surface is constrained by
[GATE-tau-provider-codex-responses-surface-selection](../../tau-provider-codex/specs/GATE-tau-provider-codex-responses-surface-selection.md).

The main runtime loop owns ChatGPT quota profile epochs and reconciliation.
Prompt workers only report normalized observations through the worker channel;
quota failures neither delay inference nor consume prompt retry budget. This
follows
[GATE-provider-quota-pacing](../../../specs/GATE-provider-quota-pacing.md).

## Provider and credential boundary

External provider responses are untrusted prompt-surface data. Streamed text,
reasoning, tool arguments, and custom-tool input must not be copied into status,
notices, traces, or final transcript rendering. Public response stats are
content-free, prompt-local transport metadata; the extension owns their sampling
and the harness validates and broadcasts them under
[SPEC-provider-response-streaming](../../../specs/SPEC-provider-response-streaming.md).
Provider execution output uses transient `_reported` events, from which the
harness derives canonical facts under
[SPEC-provider-execution-reports-and-canonical-facts](../../../specs/SPEC-provider-execution-reports-and-canonical-facts.md).

The provider boundary cannot authorize transcript mutation or compaction.
Built-in providers may classify a context-window failure, but the harness clears
provider-supplied recovery claims and independently derives recovery eligibility
from its prompt, model, operation, policy, and branch state.

Profile files, OAuth tokens, and API keys are local secrets. They must not enter
model-visible output, notices, traces, debug logs, or fixtures. Debug captures
may contain full prompt and tool-result content and therefore remain gated by
explicit durable-session policy. New captures are zstd-compressed on one
bounded best-effort background writer; overload, write failure, or process
shutdown can omit captures but never delay provider or UI work. Detailed
credential and response controls are owned by [`SECURITY.md`](../SECURITY.md).

## Runtime and worker flow

Protocol startup publishes provider kind and subscriptions, declares models,
and then signals readiness. The harness derives canonical model state after
activation. The normal `tau-client` writer is the only protocol serialization
path.

Prompt and prewarm workers enqueue typed messages before waking the manual main
loop with `ManualRuntimeWaker`. Wakes are coalesced, so the loop drains both
harness input and worker messages before blocking again. Prewarm work is
supervised separately from prompt concurrency. Delayed retries share one
scheduler thread and do not retain worker permits; their ownership and cooldown
contract is specified by
[SPEC-tau-ext-provider-builtin-retry-scheduler](SPEC-tau-ext-provider-builtin-retry-scheduler.md).

Prompt cancellation is cooperative. Queued work can be removed immediately,
retry sleeps can be aborted, and backends may register prompt-specific transport
wakers. Input EOF stops new work while allowing active workers to flush; explicit
disconnect or shutdown cancels pending work and wakes registered transports.
After terminal disconnect, detached workers retain finite network bounds but no
longer reconcile into the dropped runtime loop.

Adapters classify typed terminal provider failures independently of display
text. Terminal failures bypass retries, and raw provider bodies never become
closed failure categories. Watcher projection of bounded provider work is
governed by [SPEC-agent-watch](../../../specs/SPEC-agent-watch.md).
