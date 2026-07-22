# tau-ext-test-dummy security and reliability boundary

`tau-ext-test-dummy` is a disabled-by-default, test-only configured extension.
It is trusted same-UID local fixture code, not a sandbox and not user-facing
functionality. It communicates only through Tau's stdio extension protocol. The
supervisor may provide ordinary inherited process state, but this crate does not
inspect environment variables or secrets, use its configured state directory,
perform path-based filesystem I/O, open network connections, or spawn child
processes.

The harness supplies typed configuration, prompt events, tool starts,
cancellation, and disconnect. Configuration rejects unknown fields and invalid
`restart_mode` values. The registered tool has an empty object schema.
Replay-marked starts cannot execute tool behavior. Prompt interception may
replace only text and must preserve identity, routing, class, originator,
display metadata, and context identity.

`hold_no_side_effect` owns at most one worker. It publishes correlated readiness
only after worker startup, accepts cancellation only for the exact active call,
has a fixed terminal deadline, and joins the worker on cancellation, disconnect,
or teardown. Cancellation may emit only the correlated cancellation terminal;
disconnect and teardown emit none. The mode must never gain model arguments,
runtime control, filesystem, network, environment, secret, or child-process
authority.

Primary safeguards are the focused dummy lifecycle tests and the S6
interrupted-worker cold-resume acceptance in `tau-e2e-tests`. Re-review this
boundary when changing configuration, tool arguments, worker/thread ownership,
deadlines, cancellation or disconnect ordering, replay handling, protocol
output, process capabilities, or fixture enablement.
