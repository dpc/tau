---
name: tau-self-knowledge-ext-test-dummy
description: Use this extension skill when the user asks about Tau's test-dummy extension, restart_test_dummy, test-only extension restart behavior, prompt interception tests, or deterministic dummy extension configuration.
advertise: false
---

# Tau test-dummy extension self-knowledge

`test-dummy` is a disabled-by-default test extension. It runs `tau-ext-test-dummy` and exists to exercise harness extension supervision, tool dispatch, restarts, config errors, and prompt interception behavior.


## Features

- Registers `restart_test_dummy`, a tool that historically either exits the extension process or returns an error at random.
- Can be configured with deterministic `restart_mode` for tests: `random`, `success`, `error`, or `exit`.
- Intercepts `agent.prompt_submitted` and rewrites whole-word `tao` to `tau`, preserving letter case. When it changes text it emits a transient harness notice message: `did you mean "Tau"? — corrected for you`.

This extension is not intended as user-facing functionality. It should stay disabled in normal configs.


## Configuration

Configured under `extensions.test-dummy.config` when explicitly enabled:

```json5
extensions: {
  "test-dummy": {
    enable: true,
    config: {
      restart_mode: "success", // random | success | error | exit
    },
  },
}
```
