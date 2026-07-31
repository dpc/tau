---
name: tau-self-knowledge-ext-test-dummy
description: Use this extension skill when the user asks about Tau's test-dummy extension, restart_test_dummy, test-only extension restart behavior, prompt interception tests, or deterministic dummy extension configuration.
advertise: false
---

# Tau test-dummy extension self-knowledge

`test-dummy` is a disabled-by-default test extension. It runs `tau-ext-test-dummy` and exists to exercise harness extension supervision, tool dispatch, restarts, config errors, and prompt interception behavior.


## Features

- Registers `restart_test_dummy`, a tool that historically either exits the extension process or returns an error at random.
- Can be configured with deterministic `restart_mode` for tests: `random`,
  `success`, `error`, `exit`, `hold_no_side_effect`, or
  `hold_until_success_release`.
- `hold_no_side_effect` accepts one no-argument invocation, emits correlated
  readiness progress after its bounded worker starts, and performs no
  filesystem, network, environment, or child-process operation. It joins on
  cancellation/disconnect and has a fixed ten-second terminal deadline.
- `hold_until_success_release` requires the caller to own a private `0700` root,
  choose a fresh absent socket leaf, and generate a nonce for each invocation.
  The extension binds and removes only that leaf, reports readiness before
  arming release, and accepts 4096-byte-bounded newline-delimited JSON frames
  containing the exact active `call_id` and configured `release_nonce`. An exact
  match returns the normal `restart succeeded` result; bad frames do not release
  it. One overall deadline bounds connection and frame assembly, while
  cancellation/disconnect wakes and joins all worker threads without synthetic
  success.
- Ignores replay-marked `tool.started` deliveries so historical restart events do not emit tool replies or exit the extension again; malformed config is surfaced as `ConfigError`.
- Intercepts `agent.prompt_submitted` and rewrites whole-word `tao` to `tau`, preserving letter case. When it changes text it emits a transient harness notice message: `did you mean "Tau"? — corrected for you`.

This extension is not intended as user-facing functionality. It should stay disabled in normal configs.


## Configuration

Configured under `extensions.test-dummy.config` when explicitly enabled:

```json5
extensions: {
  "test-dummy": {
    enable: true,
    config: {
      restart_mode: "success", // also hold_no_side_effect | hold_until_success_release
      // Release mode additionally requires:
      // release_socket_path: "/private/fixture/release.sock",
      // release_nonce: "fixture-generated-nonce",
    },
  },
}
```
