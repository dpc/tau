# tau-ext-test-dummy security and reliability boundary

`tau-ext-test-dummy` is a disabled-by-default, test-only configured extension.
It is trusted same-UID local fixture code, not a sandbox and not user-facing
functionality. It communicates through Tau's stdio extension protocol and, in
release mode only, one caller-provisioned fixture-private Unix socket. The
supervisor may provide ordinary inherited process state, but this crate does not
inspect environment variables or secrets, use its configured state directory,
open network connections, or spawn child processes. The one narrow filesystem
exception is the configured fixture-private Unix socket used by
`hold_until_success_release`.

The harness supplies typed configuration, prompt events, tool starts,
cancellation, and disconnect. Configuration rejects unknown fields and invalid
`restart_mode` values. The registered tool has an empty object schema.
Replay-marked starts cannot execute tool behavior. Prompt interception may
replace only text and must preserve identity, routing, class, originator,
display metadata, and context identity.
`typed_image` returns only a compiled fixed 1×1 PNG as native typed result
content. It accepts no arguments or runtime control and adds no image decoding,
filesystem, network, environment, secret, or child-process authority.

`hold_no_side_effect` owns at most one worker. It publishes correlated readiness
only after worker startup, accepts cancellation only for the exact active call,
has a fixed terminal deadline, and joins the worker on cancellation, disconnect,
or teardown. Cancellation may emit only the correlated cancellation terminal;
disconnect and teardown emit none. The mode must never gain model arguments,
runtime control, filesystem, network, environment, secret, or child-process
authority.

For each invocation, the caller creates and owns a private `0700` fixture root,
chooses a fresh absent socket leaf beneath it, and generates the nonce.
`hold_until_success_release` binds that leaf and removes it on every worker exit;
it never creates or removes the caller's root. Its newline-delimited JSON frame
is capped at 4096 bytes including the delimiter and contains only `call_id` and
`release_nonce`.
Both values must exactly match the active invocation and configured
caller-generated nonce. Bad, oversized, stale, duplicate, or mismatched frames
are rejected without release. The worker accepts one active invocation, reports
readiness before arming release, and uses one bounded overall lifecycle.
Cancellation and shutdown wake notification-driven waits. Teardown joins all
owned threads and removes the socket without manufacturing a success result.

Primary safeguards are the focused dummy lifecycle tests and the S6
interrupted-worker cold-resume acceptance in `tau-e2e-tests`. Re-review this
boundary when changing configuration, tool arguments, worker/thread ownership,
deadlines, cancellation or disconnect ordering, replay handling, protocol
output, process capabilities, or fixture enablement.
