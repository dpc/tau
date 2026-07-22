# ARCH-tau-ext-test-dummy: tau-ext-test-dummy architecture

`tau-ext-test-dummy` is a disabled-by-default fixture extension for Tau harness
tests. It is not user-facing functionality.

The extension is intentionally small:

- it performs the standard stdio extension handshake;
- it registers `restart_test_dummy` in the `test` tool group;
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
`hold_no_side_effect` is a closed lifecycle mode: it acknowledges one live
invocation with correlated transient progress only after a worker reaches its
wait point, then waits for exact cancellation or a fixed terminal deadline.
Cancellation joins the worker and reports cancellation; disconnect and teardown
join it without terminal output.

Restart replies (`success` `tool.result_reported` and error
`tool.error_reported`) must echo the
incoming `ToolStarted.originator`; they must not synthesize
`PromptOriginator::User`, because extension-originated invocations rely on the
originator for correct routing/classification.

This crate is a local test fixture and should remain disabled in normal user
configuration.

Security-relevant boundaries:

- Communication is limited to the trusted local Tau extension stdio protocol.
- The crate does not read or write files, persist state, access secrets, open
  network connections, or spawn subprocesses.
- Config parsing rejects unknown fields and invalid `restart_mode` values.
- Hold mode accepts no tool arguments or external control, permits one active
  worker, bounds readiness at one second and terminal waiting at ten seconds,
  and joins that worker on cancellation or shutdown.
- Replayed `tool.started` deliveries are ignored so historical events cannot
  retrigger tool replies or extension exits.

Prompt interception is deliberately narrow. It changes only prompt text and
must preserve routing and identity fields so hidden/internal or extension
originated prompts do not become user-visible prompts by accident.

The cross-crate interrupted-tool acceptance that consumes the hold mode is
specified by
[SPEC-tau-e2e-deterministic-provider](../../tau-e2e-tests/specs/SPEC-tau-e2e-deterministic-provider.md).
