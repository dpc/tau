# tau-client architecture

`tau-client` is the shared runtime slice for Tau protocol peers. It sits above
`tau-proto`, which owns the wire messages, and alongside existing
`tau-extension` handshake code so extensions can migrate incrementally without a
protocol break.

The runner writes startup frames in harness-defined order: `Hello`, optional
`Subscribe`, optional `Intercept`, startup `Emit` frames, then `Ready`. After
startup it reads harness messages and dispatches configuration, deliveries,
intercept requests, and disconnects.

Outbound frames go through a writer thread owned by `TauExtensionRunner`.
`ClientHandle` is cloneable, serializes writes through that thread, and waits for
each frame to be encoded and flushed before returning. A closed or panicked
writer is reported as `ClientError`.

Event handlers are either typed payload handlers or raw delivery handlers. Typed
handlers cover the built-in `EventPayload` variants; raw handlers are available
for unsupported first-party or custom extension events. Replay-aware handlers
receive both historical and live deliveries, while live-only handlers skip
replay-marked deliveries.

Configuration handlers deserialize the CBOR configuration into the requested
type. Decode failures and handler application errors emit `ConfigError` frames
and the runner continues processing later messages.

Intercept handlers always produce exactly one `InterceptReply` for each request.
If the handler fails, the runner sends a pass-through reply first, then returns
the handler error so the extension run stops without leaving the harness waiting.

Tests in `src/tests.rs` cover startup ordering, config errors, replay/live event
dispatch, raw event dispatch, tool-name matching, intercept reply guarantees,
disconnect behavior, builder validation, empty subscriptions, and plugin
composition.
