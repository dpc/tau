# SPEC-provider-cache-refresh-lifecycle: Bounded Provider cache refreshes

## Record justification

Refresh ownership spans harness scheduling, directed IPC, Provider worker
supervision, terminal correlation, and privacy boundaries, so no single crate
can state the complete lifecycle contract.


## Authority and routing

The disabled-by-default harness scheduler is the sole owner of refresh
eligibility, evidence, concurrency, and lifecycle correlation. It captures the
exact Provider connection, model route, agent, prefix
identity version, and process-secret full-prefix digest. Sensitive
`agent.cache_refresh_requested` and `agent.cache_refresh_cancel_requested`
events use only point-to-point delivery to that captured connection.

The request carries a validated random `pcr-*` identifier, the exact approved
prefix, and `stop_after_millis` in `1..=30000`. It is transient, nonpersistent,
nonreplayed, and excluded from interception, broadcast, generic diagnostics,
watchers, and UI. Content-free Provider terminal reports use one of `succeeded`,
`failed`, `cancelled`, `unsupported`, or `deadline_exceeded`.


## Ordering and terminal ownership

The harness reserves capacity, installs ownership, revalidates the route, then
enqueues the directed request. A real prompt enqueues cancellation before the
prompt on the same Provider FIFO and never waits. The Provider synchronously
invalidates the matching supervisor entry before starting later prompt work.

The exact current Provider generation may report one terminal. The harness
releases capacity only after that authenticated terminal, a definitive
pre-receipt enqueue failure, authenticated disconnect, the exact stop deadline,
or shutdown. Sending cancellation alone does not release capacity. Duplicate,
unknown, mismatched, and stale terminals have no semantic effect. Failures never
retry and cooldown rejection produces one terminal.


## Admission and invalidation

Automatic scheduling requires a safe published cache contract: automatic prefix
or explicit breakpoint, known sliding TTL, read renewal, zero output, volatile
ZDR-compatible route-preserving storage, concrete quota charges, and explicit
uncached/read/write prices. It requires an observed write followed by
`max(1, ceil(max(W-U, 0)/(U-R)))` reads and `2R <= W`.

Each later qualifying read creates a new observation generation and reschedules
at most one attempt for that generation. The effective horizon is the lesser of
TTL and configured idle duration. Jitter is zero below ten seconds, otherwise
uniformly `1..=min(30, horizon/10)` seconds, and dispatch is `stop-jitter`.
Evidence is bounded to 1,024 global and 128 per Provider with deterministic
oldest eviction.

Refreshes currently run only while a registered foreground tool cohort remains
unsettled, with the window deadline equal to the earliest candidate stop.
Backgrounding, cancellation, settlement, session or agent teardown, route,
Provider, model, tool, system-prompt, prefix, or policy change closes or
invalidates applicable work. Approval waits admit nothing until Tau has a typed
finite-deadline approval operation. No scheduler state is journaled or restored.
