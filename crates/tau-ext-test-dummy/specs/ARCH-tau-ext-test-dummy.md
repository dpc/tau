# ARCH-tau-ext-test-dummy: tau-ext-test-dummy architecture

`tau-ext-test-dummy` is a disabled-by-default fixture extension for Tau harness
tests. It is not user-facing functionality.

The extension is intentionally small:

- it performs the standard stdio extension handshake;
- it registers `restart_test_dummy` and the image-only
  `typed_image_test_dummy` in the `test` tool group;
- it subscribes to `tool.started` and ignores replay-marked deliveries before
  performing restart/tool-result side effects;
- it intercepts `agent.prompt_submitted` and rewrites whole-word ASCII `tao` to
  `tau`.

The prompt interceptor must preserve prompt identity and routing fields
(`agent_id`, `message_class`, `originator`, display metadata, and context id)
and only change `text`.

The `restart_mode` config exists to make harness tests deterministic:
`random` preserves the historical random exit-or-error behavior, while
`success`, `error`, and `exit` force the corresponding outcome.
`typed_image: true` enables `typed_image_test_dummy`, which returns one fixed
in-memory 1×1 PNG as native typed provider content plus a
bounded text result; it exists only for the deterministic durable-replay
acceptance and does not read, decode, or write image files.
`hold_no_side_effect` is a closed lifecycle mode: it acknowledges one live
invocation with correlated transient progress only after a worker reaches its
wait point, then waits for exact cancellation or a fixed terminal deadline.
Cancellation joins the worker and reports cancellation; disconnect and teardown
join it without terminal output.
`hold_until_success_release` is a separate test-fixture mode. For each invocation,
the caller creates and owns a private `0700` root, configures a fresh absent
`release_socket_path` below it, and generates `release_nonce`. The extension
binds and later removes only the socket leaf. It starts one worker, reports
readiness, and only then arms release. It accepts newline-delimited JSON frames of at
most 4096 bytes (including the newline), with the exact closed shape
`{"call_id":"…","release_nonce":"…"}`. Only an exact match for the active call
and configured nonce reports the normal `restart succeeded` result. Malformed,
oversized, duplicate, stale, and mismatched frames cannot release the hold.
One bounded overall lifecycle covers readiness, connection, and frame assembly.
Cancellation and shutdown wake the notification-driven worker. Cancellation,
disconnect, and teardown join all owned threads and remove the socket without
synthesizing success.

Both hold modes transfer their selected result, error, deadline, or cancellation
outcome to the protocol loop over an unbounded internal channel. The loop keeps
the active hold ownership until checked ordered terminal publication succeeds.
Publication failure exits the extension so harness disconnect cleanup settles
the retained call. Readiness progress remains optional detached output.

Restart replies (`success` `tool.result_reported` and error
`tool.error_reported`) must echo the
incoming `ToolStarted.originator`; they must not synthesize
`PromptOriginator::User`, because extension-originated invocations rely on the
originator for correct routing/classification.

This crate is a local test fixture and should remain disabled in normal user
configuration.

Security-relevant boundaries:

- Communication is limited to the trusted local Tau extension stdio protocol
  and the fixture-private Unix release socket described above.
- Apart from creating and removing that socket, the crate does not read or
  write files, persist state, access secrets, open network connections, or spawn
  subprocesses.
- Config parsing rejects unknown fields and invalid `restart_mode` values.
- `typed_image_test_dummy` is foreground-only and accepts no arguments or
  runtime control; it appears only when `typed_image: true`.
- `hold_no_side_effect` accepts no tool arguments or external control, permits
  one active worker, bounds readiness at one second and terminal waiting at ten
  seconds, and joins that worker on cancellation or shutdown.
- `hold_until_success_release` permits one externally released worker and
  arbitrates release, cancellation, and shutdown so exactly one outcome wins.
- Replayed `tool.started` deliveries are ignored so historical events cannot
  retrigger tool replies or extension exits.

Prompt interception is deliberately narrow. It changes only prompt text and
must preserve routing and identity fields so hidden/internal or extension
originated prompts do not become user-visible prompts by accident.

The cross-crate interrupted-tool acceptance that consumes the hold mode is
specified by
[SPEC-tau-e2e-deterministic-provider](../../tau-e2e-tests/specs/SPEC-tau-e2e-deterministic-provider.md).
