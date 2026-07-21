# SPEC-terminal-output-side-effect-events: Live terminal-output side effects

## Record justification

Terminal-output event authority, extension activation, harness persistence,
terminal UI replay handling, and the first-party notification producer span
separate crates. This record keeps that distributed live-only contract coherent.

## Authority and publication

`term.osc1337_set_user_var` and `term.bell` retain their existing wire names.
Every authenticated live configured extension entry kind, including configured
Core, may author either event without a capability. An attached harness-assigned
socket UI may also author them. Unconfigured or disconnected extension
connections and dedicated external-message socket peers have no authority.
Harness-internal direct publication remains outside peer admission.

Admission performs no terminal side effect. Accepted peer events use ordinary
generic Emit interception, commit, and broadcast with their authenticated source
and caller-selected `Emit.transient` unchanged. Interceptors may drop an event or
replace it with the same event name; a dropped event never reaches a terminal UI.

## Activation and persistence

Pre-Ready extension frames are globally ordered operational traffic, not
activation declarations. They remain in the bounded deferred-message queue until
the source and global activation barrier permit publication, then commit in
original arrival order.

Both events are live side effects. They retain runtime sequencing, debug
publication, interception, and live broadcast, but never enter agent, session, or
restore semantic stores for either caller-selected transient value. Existing
producers may therefore retain their current wire metadata without making these
events replayable.

## Terminal UI consumption

Terminal UIs subscribe to both event names only for live delivery. They must also
reject replay-marked deliveries before invoking terminal output, independently of
subscription filtering. A replay must never repeat a bell or OSC escape sequence.

The CLI base64-encodes OSC values and validates OSC names before writing terminal
bytes. `tau-ext-std-notifications` is the first-party producer: it validates
rendered names, bounds rendered values, and retains its existing non-transient
Emit metadata. These validations are defense in depth and do not replace harness
event authority.

## Scope

This specification implements only the terminal-output row of
[DECISION-generic-peer-event-emission](DECISION-generic-peer-event-emission.md).
It does not change terminal DTOs, OSC encoding, notification triggers, extension
configuration, or any tool, action, shell, or UI-command authority row.
