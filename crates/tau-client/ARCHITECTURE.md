# tau-client architecture

`tau-client` is the shared runtime slice for Tau protocol peers. It sits above
`tau-proto`, which owns the wire messages, and alongside existing
`tau-extension` handshake code so extensions can migrate incrementally without a
protocol break.

The runner writes startup frames in harness-defined order: `Hello`, optional
`Subscribe`, optional `Intercept`, startup `Emit` frames, then `Ready`. After
startup it reads harness messages and dispatches configuration, deliveries,
intercept requests, and disconnects.

The manual-loop runtime uses the same startup writer and dispatch machinery, but
hands receive-loop ownership to the extension. It starts a reader thread, exposes
`recv`/`recv_timeout` so extensions can select between harness input and local
timers, and exposes `dispatch_one` for decoded messages. Extension state and
handlers stay on the caller thread; only the protocol reader and writer need to
move to background threads.

The builder preserves the first-seen order of startup subscription selectors
while coalescing exact structural duplicates. It does not collapse logical
overlaps such as `Prefix("tool.")` plus an exact `tool.started` selector.

Outbound frames go through a writer thread owned by `TauExtensionRunner`.
`ClientHandle` is cloneable, serializes writes through that thread, and waits for
each frame to be encoded and flushed before returning. A closed or panicked
writer is reported as `ClientError`. Detached enqueue helpers are also available
for background workers that must not block the protocol reader on output
backpressure; those helpers report only queue-closed failures to the caller, then
let the writer thread own any later encode/flush error.

Extensions that intentionally leave background workers running after disconnect
can opt into a detached-writer run mode. That mode preserves startup and handler
error reporting but does not join the writer at shutdown, so harness
`Disconnect` latency does not depend on queued background output.

Extensions whose background workers need a persistent outbound handle can use
the detached-writer state-factory entry point. The runner writes the complete
startup prelude through `Ready` before invoking the factory with a cloneable
`ClientHandle`, preserving startup staging while letting runtime state retain a
handle for later worker output.

Manual-loop extensions use the same startup staging. Their state factory also
runs only after `Ready`, and timer branches can use `ManualExtensionRuntime`'s
separate `handle()` method to emit output without storing a handle in every
state type.

Event handlers are either typed payload handlers or raw delivery handlers. Typed
handlers cover the built-in `EventPayload` variants; raw handlers are available
for unsupported first-party or custom extension events. Replay-aware handlers
receive both historical and live deliveries, while live-only handlers skip
replay-marked deliveries.

Configuration handlers deserialize the CBOR configuration into the requested
type. Decode failures and handler application errors emit `ConfigError` frames
and the runner continues processing later messages. Handlers can register a
configuration-error hook for fail-closed cleanup; that hook runs before
`ConfigError` is emitted for both typed decode failures and application
failures.

Extensions whose lifecycle policy must run before typed decoding can register a
raw configuration handler. Raw handlers receive the original `Configure` message
and can parse it explicitly after checking runtime state; returned errors still
emit one `ConfigError` and do not stop the message loop.

Manual-loop receive results distinguish timeout, clean input EOF, and protocol
`Disconnect`. Clean input EOF allows the caller to keep running local timers and
emit post-EOF output before graceful writer shutdown. `finish()` always shuts
down and joins the writer; it joins the reader only after EOF or another already
finished reader state, because arbitrary blocking `Read` implementations cannot
be cancelled portably. If a caller stops before EOF, the blocked reader is
detached and exits after its next read completes. Protocol `Disconnect` is a
dispatch outcome; callers that may have blocked or long-lived background output,
or a still-open input stream during disconnect, can choose detached finish, which
returns state without shutting down or joining protocol threads.

Intercept handlers always produce exactly one `InterceptReply` for each request.
If the handler fails, the runner sends a pass-through reply first, then returns
the handler error so the extension run stops without leaving the harness waiting.

## Testing strategy

Tests in `src/tests.rs` are protocol contract tests for the reusable runtime.
They should cover startup frame ordering, subscription selector semantics,
writer-thread behavior, configuration errors, replay/live dispatch boundaries,
raw event dispatch, tool-name matching, intercept reply guarantees, disconnect
behavior, manual-loop receive/dispatch/shutdown contracts, builder validation,
empty subscriptions, and plugin composition. Add focused coverage when changing
lifecycle, writer shutdown, replay filtering, configuration, intercept, manual
receive loops, or startup declaration behavior so migrated extensions do not
need to rediscover runtime regressions independently.
