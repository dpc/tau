# DESIGN-tau-ext-provider-builtin-bounded-prompt-workers: Prompt execution uses bounded workers with cooperative cancellation

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
waker. Broadcast cancellation aborts all active retry sleeps and wakes every
registered backend abort waker. Disconnect/shutdown likewise aborts retry sleeps
and wakes all registered backend abort wakers. Backend network reads remain
transport-owned and cancellation is cooperative rather than a hard socket
interruption; ChatGPT/Codex WebSocket turns use the abort-waker path to wake an
idle provider-event receive and return the normal harness cancellation result
promptly. Broadcast cancellation and abort-waker registration linearize under
the same cancellation mutex: either cancellation snapshots an existing waker or
a later old-generation registration observes the generation change and wakes
immediately. Callbacks run after releasing the mutex.

Best-effort ChatGPT WebSocket prewarm runs on separately supervised finite
workers and never occupies the manual runtime loop or a prompt-worker permit.
The main loop admits at most one worker per provider/target-agent cache owner
and at most the default WebSocket pool capacity across the process,
owns its cancellation until an exact generation-tagged completion, and cancels
it for a matching real prompt, cancel, shutdown, disconnect, or changed
credential/profile identity. Prewarm transport itself supplies the finite
network deadlines; completion returns through the enqueue-before-wake worker
channel while the runtime remains attached. Terminal transport disconnect
cancels all work and drops the loop-side registry; each detached worker retains
its own cancellation source and finite transport bounds until process exit or
completion.

Unlike prompt cancellation callbacks, prewarm abort callbacks execute while
the prewarm callback registry is locked. They are restricted to nonblocking
transport wake enqueue or WebSocket-pool invalidation and must not re-enter the
abort registry. Dropping the pool-invalidation callback guard is therefore the
logical socket-publication boundary: it either waits for an already-started
cancellation callback to finish or unregisters before later cancellation begins.

ChatGPT/Codex fresh WebSocket upgrades also register the prompt abort waker and
are independently deadline-bounded. Before that work begins, the provider emits
only the fixed `Connecting to provider…` status; endpoint, account, credential,
and raw transport diagnostics remain provider-local.

Built-in providers batch `provider.response_updated` output. They may emit the
first non-empty streamed output sample promptly, then emit later non-terminal
progress at most once per second per prompt. Worker/backend stream loops must not
write Tau protocol progress events directly from every upstream chunk; transport bytes, visible deltas, compaction status, and public content-free
response stats are accumulated until the rate-limited emitter samples them,
except for the first non-empty sample and for a terminal flush immediately before
the provider prompt closes.
