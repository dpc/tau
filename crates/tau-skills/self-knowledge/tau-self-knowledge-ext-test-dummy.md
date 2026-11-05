---
name: tau-self-knowledge-ext-test-dummy
description: Use when testing Tau test-dummy, including restart_test_dummy, test-only extension restart behavior, prompt interception tests, or deterministic dummy extension configuration.
advertise: false
---

# Test-only dummy extension

`test-dummy` runs `tau-ext-test-dummy` for supervision, dispatch, restart, and
interception tests. Keep it disabled in normal configurations.


## Features

- `restart_test_dummy` supports `restart_mode`: `random`,
  `success`, `error`, `exit`, `hold_no_side_effect`, or
  `hold_until_success_release`, or `exit_once_then_success`.
- `configure_user_data_probe: true` writes fixed `configured` bytes to the
  User-scope `configure-probe` leaf during Configure, to test ephemeral startup.
  Neither path nor content is configurable.
- `typed_image: true` enables no-argument `typed_image_test_dummy`: a fixed
  in-memory 1×1 PNG for durable-replay tests, using captured Inline/Artifact
  policy and the SDK uploader, without image-file I/O.
- `provider_context_raw_message: true` enables one test-only tool that publishes
  an existing message-bridge delivered report to its exact `agent_id` and `text`
  arguments. Gated provider-context placement E2Es are its only intended caller.
- `hold_no_side_effect` accepts one no-argument call, reports correlated
  readiness, performs no external operations, joins on cancellation/disconnect,
  and has a ten-second terminal deadline.
- `hold_until_success_release` requires the caller to own a private `0700` root,
  choose a fresh absent socket leaf, and generate a nonce for each invocation.
  It binds/removes only that leaf and reports readiness before arming release.
  Release requires a newline-delimited JSON frame of at most 4096 bytes matching
  `call_id` and `release_nonce`; success returns `restart succeeded`.
  Readiness/frame reads are bounded, but release has no competing timer.
  Cancellation/disconnect joins workers; listener/configuration errors fail
  closed without synthetic success.
- `exit_once_then_success` requires one absolute marker below a caller-owned
  private `0700` fixture root. The first call reports progress, atomically creates
  the absent marker, and exits without a terminal. The replacement checks the
  regular marker and returns `restart succeeded`. Invalid config, missing parent,
  non-regular marker, or filesystem errors fail closed. No timing/fault scripting.
- Ignores replay-marked `tool.started` deliveries so historical restart events do not emit tool replies or exit the extension again; malformed config is surfaced as `ConfigError`.
- Intercepts `agent.prompt_submitted` and rewrites whole-word `tao` to `tau`, preserving letter case. When it changes text it emits a transient harness notice message: `did you mean "Tau"? — corrected for you`.

## Configuration

Configured under `extensions.test-dummy.config` when explicitly enabled:

```json5
extensions: {
  "test-dummy": {
    enable: true,
    config: {
      configure_user_data_probe: false,
       restart_mode: "success", // also hold_no_side_effect | hold_until_success_release | exit_once_then_success
      typed_image: false, // enables typed_image_test_dummy
      // Release mode additionally requires:
       // release_socket_path: "/private/fixture/release.sock",
       // release_nonce: "fixture-generated-nonce",
       // Exit-once mode additionally requires:
       // exit_once_marker_path: "/private/fixture/exit-once-marker",
    },
  },
}
```
