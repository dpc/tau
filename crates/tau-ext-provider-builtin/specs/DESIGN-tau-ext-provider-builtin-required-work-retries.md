# DESIGN-tau-ext-provider-builtin-required-work-retries: Required provider work retries outside the worker pool

Status: confirmed, 2026-07-12, user

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

An explicit user `/retry` may atomically remove one exact `AgentPromptId` from
the delayed scheduler before its deadline. This deliberately shortens even a
trusted server delay for that job only. The same owned job and retry accounting
are preserved, peers remain parked, and the released job bypasses the shared
cooldown once before entering the normal bounded worker queue. If that attempt
commits a successful terminal response, it authoritatively invalidates the exact
provider-profile cooldown generation it probed. Matching parked peers advance
with stable prompt-local anti-herd jitter; unrelated providers remain untouched.
Cancellation, failure, and a success from an older generation do not release a
current cooldown. Replacing the configured profile identity also invalidates the
old profile's cooldown, while best-effort quota display telemetry never does.
The telemetry non-authority follows
[DECISION-provider-quota-pacing](../../../specs/DECISION-provider-quota-pacing.md).

Scheduler mutation is implemented as synchronous, single-owner command
transitions plus an explicit monotonic-time advance, both returning ownership
actions for the provider actor to deliver. The production actor adds only
channel transport and timer waiting. Tests may inject and advance the
monotonic clock, so multi-day cooldown, exact-generation release, independent
deadlines, and anti-herd wakeup are acceptance-tested without network access,
wall sleeps, or quota telemetry acting as scheduler policy.

The synchronous transition seam is guarded by a second, independent delayed
ownership/deadline reference model. Bounded fixed-seed command traces run on
every change and cover schedule, extend, release, manual transfer, cancellation,
virtual advance, and duplicate identities. Conservation, exact
provider/generation scope, independent deadline preservation, and bounded
progress after a valid release are checked after every scheduler command.
Runtime fixtures—not synthetic queue commands—own profile rotation, telemetry
non-authority, cancellation/commit, and provider-side shutdown/EOF.
Property failures report their seed and the minimized replayable trace;
scheduled CI may raise the case budget without changing command semantics or
introducing wall-clock scheduling oracles.

Authority amendment (confirmed by the user for Stage 2): property generation is
limited to commands owned by the pure scheduler actor. Profile rotation, quota
telemetry, and EOF/shutdown instead use deterministic production-runtime
fixtures, where their actual control-plane semantics live. This layered split
must not be replaced with synthetic queue-level stand-ins.
