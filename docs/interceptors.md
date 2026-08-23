# Event interceptors

Event interceptors let a peer handle an event emission before the event is
appended to the harness event log and before normal subscribers see it.

This is a pre-log emission pipeline. While an event is intercepted, it is
considered not emitted yet.

## Events vs messages

There are two protocol layers:

- **Events** are bus facts. They have dotted `category.call` names, are appended
  to the event log when committed, and are delivered to subscribers inside
  `deliver` messages.
- **Messages** are point-to-point protocol traffic. They have flat
  single-component `snake_case` names and are sent between the harness and one
  peer.

Interception is controlled with messages, but it acts on event emissions.

The relevant messages are:

- `intercept` — peer → harness registration request
- `emit` — peer → harness request to publish an event
- `intercept_request` — harness → interceptor delivery of an intercepted,
  not-yet-emitted event
- `intercept_reply` — interceptor → harness decision for that request

The event inside `emit` or `intercept_request` is the event being processed. The
message itself is not the emitted fact.

Some peer events are requests rather than canonical facts. For example,
`agent.metadata_set_request` commits through this pipeline before the harness
validates it and publishes a distinct `agent.metadata_set` fact. Interceptors can
therefore observe both commits. A set request carrying `mutation_id` is
non-droppable and retains its agent, key, correlation, and inheritance identity
across replacement. Request replacements commit and undergo downstream
validation. Interception alone cannot drop or retarget a correlated request.
Downstream validation, stale-source rejection, or store failure may still
produce no canonical echo.

## Registering an interceptor

A peer registers interception interest with the `intercept` message.

The message contains:

- `selectors`: event selectors to intercept
  - exact event names
  - prefixes
- `priority`: interception priority

Lower numeric priority values run first.

Registrations are owned by the connection that sent them. When that connection
disconnects or crashes, the harness removes its interceptor registrations. If
the peer reconnects, it must register again during handshake.

## Matching

When the harness is about to commit an event, it checks the interceptor
registry.

Exact selectors are preferred over prefix selectors. This preference is stronger
than priority. For example, an exact interceptor at priority `100` runs before a
prefix interceptor at priority `-100` for the same event.

Within the selected exact or prefix group, handlers are ordered by:

1. priority, ascending; lower values first
2. component name, ascending lexicographic order
3. connection id, ascending, as a final deterministic fallback

Component names are expected to be unique. Runtime enforcement is still TODO.

## Intercept request delivery

If a matching interceptor exists, the harness does not append the event to the
event log and does not deliver it to subscribers yet.

Instead, the harness sends the selected interceptor a directed
`intercept_request` message:

```text
intercept_request {
  event,
  persist
}
```

Fields:

- `event`: the not-yet-emitted event
- `persist`: the persistence flag that will apply when the event commits

This delivery bypasses normal subscriptions. A peer receives intercept requests
because it registered as an interceptor, not because it subscribed to
`intercept_request`. Messages are point-to-point and are not subscribable.

## Interceptor outcomes

An interceptor must reply exactly once with `intercept_reply`.

### Drop

The interceptor can reply with `drop`.

The event is consumed and never reaches later interceptors, the event log, or
normal subscribers. The harness overrides `drop` for must-pass events and
publishes the original event instead. Current default must-pass events include
user input and prompt lifecycle facts; agent response facts
(`provider.response_finished`); terminal tool completion facts (`tool.result`,
`tool.result_display`, `tool.error`, `provider.tool_result`,
`provider.tool_error`, `tool.cancelled`, `tool.background_result`,
`tool.background_result_display`, and `tool.background_error`); validated user-shell
terminal facts (`shell.command_finished`); session lifecycle facts; durable
session membership facts; `agent.started`; and harness-owned agent message
projections. Canonical external `message.*` facts are also protected; their
peer-authored `message.*_reported` inputs remain ordinarily interceptable. Treat
`crates/tau-harness/src/harness/interception.rs` as the source of truth for that
list. Individual harness call sites can also mark a publish as must-pass, as
mandatory `harness.notice` alerts and critical notices do.

### Pass unchanged

The interceptor can reply with `pass` and no replacement event.

The harness resumes the interception chain after the current interceptor, still
using the original event and persistence metadata.

### Pass modified

The interceptor can reply with `pass` and a replacement event.

Later interceptors and final subscribers usually see the modified event. The
replacement must have the same event type as the original; if it does not, the
harness logs a warning and falls back to the original event. Some same-type
replacements are also rejected to preserve immutable facts. For mandatory
`harness.notice` alerts and critical notices, immutable prompt lifecycle facts,
`provider.response_finished`, terminal tool completion facts including the
payload-free result display projections, session
lifecycle/membership facts, `agent.started`, and harness-owned agent message
projections, canonical external `message.*` facts, and
`shell.command_finished`, the harness publishes the original event instead. For mutable
prompt text events, replacements may edit text but cannot change routing identity
fields such as agent id or prompt metadata. The original
persistence metadata is preserved.

Peer-authored `shell.command_progress_reported` and
`shell.command_finished_reported` remain ordinary mutable, droppable
observations. Only after one commits does the harness validate its captured
configured generation and routed-command identity, then publish the protected
canonical shell fact.

## Same-priority chaining

Suppose the ordered interceptors for an event are:

```text
(priority 10, component alpha)
(priority 10, component beta)
(priority 20, component gamma)
```

Initial emission starts at the beginning, so `alpha` receives the first
`intercept_request`.

If `alpha` replies `pass`, the harness resumes strictly after `(10, alpha)`, so
`beta` receives the next `intercept_request`.

If `beta` replies `pass`, the harness resumes after `(10, beta)`, so `gamma`
receives the next `intercept_request`.

A peer cannot spoof the cursor in its reply. The harness records the pending
interceptor and advances the chain from that connection when the reply arrives.
Unexpected or duplicate replies are ignored unless the harness is waiting to
consume the one stale reply described below.

## Failure and backpressure

Only one intercepted emission is in flight at a time. Publishes that arrive while
an interceptor is pending are queued and drained after the pending interceptor
replies (or disconnects).

If the selected interceptor disconnects before replying, the harness treats that
as `pass` unchanged so an extension cannot wedge the event pipeline by going
away mid-reply. If the harness cannot deliver an `intercept_request` to the
selected interceptor, it logs the failure, removes/skips that interceptor
registration, and continues the chain instead of parking the publish.

Agent unload, session rollover, or canceled peer receive can destructively
cancel a publication after its `intercept_request` was delivered. The harness
keeps the interceptor registration but suspends the entire connection from
matching new publications until it consumes that request's uncorrelated stale
reply. A registration replacement is stored but remains suspended. The next
`intercept_reply` from the connection is consumed without applying its action
and clears suspension; exactly one reply is consumed, then normal interception
resumes. Suspension has no timeout, so a connection that never replies remains
bypassed indefinitely. Disconnect removes the suspended registration and state;
a later connection and registration start unsuspended.

## Final emission

If scanning finds no remaining matching interceptor, the harness finally commits
the event normally:

1. classify semantic persistence from `persist`, unconditional family exclusions,
   and established `persist=false` exceptions
2. append to the harness runtime event log
3. deliver the event to subscribers inside `deliver`

Only this final step makes the event visible as an emitted fact.

## Persistence metadata

The `persist` flag is carried through interception.

An interceptor can inspect it in `intercept_request`, but replies cannot change
it. The final event commits with the original persistence metadata supplied by the
initial publish.

Events that default to transient still get that default when initially emitted
through the normal harness path. While intercepted, that value is included in
`intercept_request`.

## Debugging

The harness logs interception decisions with tracing under the
`tau_harness::interception` target.

These logs are diagnostic only. They are not event-log entries and are not
visible to normal event subscribers.

## Example flow

A peer registers:

```text
intercept {
  selectors: [Exact("ui.prompt_draft")],
  priority: 0
}
```

An attached UI requests emission using the CLI's current true `persist` bit:

```text
emit {
  event: ui.prompt_draft { ... },
  persist: true
}
```

The harness finds the interceptor and sends it:

```text
intercept_request {
  event: ui.prompt_draft { ... },
  persist: true
}
```

The interceptor modifies the event and passes it on:

```text
intercept_reply {
  action: pass(ui.prompt_draft { modified ... })
}
```

If no later interceptor matches, the modified `ui.prompt_draft` event is
committed and delivered normally.
