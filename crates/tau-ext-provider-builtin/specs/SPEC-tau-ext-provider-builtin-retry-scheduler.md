# SPEC-tau-ext-provider-builtin-retry-scheduler: Required-work retry scheduler

## Record justification

Required-work retry behavior spans provider workers and adapters, the delayed scheduler actor, profile cooldown state, cancellation, and manual retry handling, so no one implementation area can own its lifecycle.

A logical prompt remains pending across retryable provider attempts until it
succeeds, is canceled, the process/session shuts down, or the unchanged request
is positively proven deterministic and invalid. Unknown remote failures retry;
classification selects cadence, shared cooldown, visible explanation, and
profile reload behavior rather than default termination.
Standalone compaction is the finite exception: transient failures use the same
classifier, jittered Fibonacci cadence, and shared provider-profile cooldown,
but the fifth total attempt terminalizes that transaction without scheduling a
sixth. Its terminal fifth-attempt evidence still extends the matching shared
cooldown for other logical prompts. Ordinary inference retains unbounded retry
authority.
The local Chat Completions summary compactor is a narrow exception: once any
semantic output has arrived, any retryable outcome terminalizes the
standalone transaction rather than risking an ambiguous summary resend. Only
pre-semantic-output failures enter the scheduler.
Provider adapters attach a machine-readable terminal failure category to the
single final `ProviderResponseFinished`. Terminal request rejections bypass the
logical-work retry scheduler and terminalize immediately.

Workers execute one finite attempt. Retryable outcomes return the logical job to
one process-lifetime delayed scheduler, releasing the bounded execution slot
before any wait. Jittered Fibonacci cadence reaches about one minute for
transport/overload/throttle and at most thirty minutes for persistent usage,
account, auth, and unknown failures. Trusted `Retry-After` or structured reset
hints remain lower bounds for other retry classes and may be later than their
generated ceiling. A usage-window reset estimate is informational: the user or
provider may restore access early, so it never extends scheduling beyond the
generated persistent-failure cadence. Prompts using one provider profile share
limit cooldowns, while cancellation remains prompt-scoped. Visible retry status
renders long trusted delays in compact minute/hour/day units while scheduling
retains the whole-second deadline. Mutable profiles and credentials are reloaded
when delayed work becomes due.

Retry state is memory-only. Cold restart intentionally does not replay an
ambiguously accepted request because doing so can duplicate output, cost, tools,
or side effects.

## Scheduler state and ownership

One process-lifetime actor exclusively owns delayed logical jobs. Its synchronous
state accepts atomic commands to schedule work with an independent deadline and
optional shared cooldown, cancel one prompt, cancel all prompts, transfer one
exact prompt for manual retry, extend a provider cooldown generation, or release
one exact generation. An explicit monotonic-time advance makes every eligible
job due; the transport actor adds only channel delivery and timer waiting.

Every transition returns ownership actions for the provider main loop: `Due`
transfers an eligible job, `Canceled` transfers a job plus the delayed ownership
count to retire, and `Manual` returns the exact optional transferred job and
request correlation. Duplicate delayed ownership fails closed by canceling the
original rather than dispatching either duplicate. Cancellation and transfer
remove ownership atomically; a missing manual target returns a correlated empty
result.

Each parked job retains its independent eligibility deadline separately from any
provider cooldown. Extending a cooldown can only delay matching work. Releasing
an exact generation removes only that constraint and applies stable prompt-local
anti-herd jitter without changing unrelated deadlines or providers.

An explicit user `:retry` may atomically remove one exact `AgentPromptId` from
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
[GATE-provider-quota-pacing](../../../specs/GATE-provider-quota-pacing.md).
