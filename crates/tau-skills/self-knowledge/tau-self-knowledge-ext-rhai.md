---
name: tau-self-knowledge-ext-rhai
description: Use this extension skill when the user asks about Tau's disabled std-rhai scripting extension, Rhai event hooks, script config, subscriptions, interceptions, host functions, or scripting limitations.
advertise: false
---

# Tau std-rhai extension self-knowledge

`std-rhai` is Tau's disabled-by-default trusted local scripting extension. It runs `tau-ext-rhai` and lets a user-provided Rhai script observe Tau events, optionally intercept matching events, and emit Tau events through a small host API. The Rust extension owns Tau protocol framing; scripts see JSON-shaped event maps matching Serde's event form.


## Configuration

Enable it under `extensions.std-rhai` and provide a script path:

```yaml
extensions:
  std-rhai:
    enable: true
    config:
      script: "/home/me/.config/tau/hooks/demo.rhai"
      vars:
        greeting: "hello"
      limits:
        max_operations: 1000000
```

`script` is required. Absolute paths are preferred; relative paths are resolved from the extension process current working directory. `vars` is arbitrary JSON-compatible data passed into `init(config)` and `start(config)`. The harness-provided `state_dir`, when present, is also passed to both callbacks.

If config parsing, script reading, compilation, or `init` fails, the extension sends `ConfigError`, then `Ready` with a `rhai disabled: ...` message, and stays alive inert instead of exiting in a restart loop.


## Script callbacks

Scripts use JSON-shaped event maps such as:

```rhai
#{ event: "harness.notice", payload: #{ kind: "extension.notice", message: "hi", level: "info" } }
```

Supported callbacks:

```rhai
fn init(config) {
    return #{
        subscribe: [#{ kind: "exact", value: "agent.prompt_submitted" }],
        intercept: [#{
            selectors: [#{ kind: "prefix", value: "agent." }],
            priority: 0,
        }],
        ready_message: "rhai ready",
    };
}

fn start(config) {
    tau_info(`rhai started with greeting: ${config.vars.greeting}`);
}

fn on_event(event, meta) {
    if event.event == "agent.prompt_submitted" {
        tau_info(`saw prompt: ${event.payload.text}`);
    }
}

fn on_intercept(event, transient) {
    event.payload.text = event.payload.text.replace("tao", "tau");
    return #{ kind: "pass", event: event };
}
```

`init(config)` is optional. Missing `init` or unit/no-op return means no subscriptions, no intercepts, and the default ready message. `subscribe` uses selector maps with `kind: "exact"` or `kind: "prefix"`. Multiple `intercept` entries are allowed only when they share the same priority; their selectors are merged into one registration because the harness supports one interceptor registration per extension connection.

`start(config)` is optional and runs once after `init` succeeds, subscriptions/intercepts are sent, `Ready` is sent, and host functions are registered. Use it for startup side effects such as `tau_info`; callback errors are reported as transient warning `harness.notice` diagnostics without disabling the extension.

`on_event(event, meta)` is optional and is called for delivered subscribed events. `meta.replay` is true for subscribe-time catch-up history, and `meta.recorded_at` is present when the harness supplies the event timestamp. Scripts with external side effects should skip replayed events.

`on_intercept(event, transient)` is optional and returns one of:

- `()` / `"pass"` / `#{ kind: "pass" }` to pass the original event.
- `#{ kind: "pass", event: event }` to pass a replacement event.
- `"drop"` / `#{ kind: "drop" }` to drop the event.

On script errors or invalid intercept returns, Tau reports a transient warning `harness.notice` diagnostic and defaults to passing the original event.


## Host functions

`register_tool_group(name, spec)` and `register_tool(name, spec, handler)` are available during `init` only. Group specs must be empty in v1. Tool specs support `description`, `parameters`, `model_visible_name`, `enabled_by_default`, and `group`; tool tags are not exposed yet, so Rhai tools are registered without tag metadata. Undeclared groups referenced by a tool are auto-created as empty groups. Handlers are Rhai function pointers such as `Fn("my_tool")` and are called as `handler(args, call_info)` for live owned `tool.started` events. `call_info` contains `call_id`, `tool_name`, `agent_id`, `originator`, and `tool_type`. Replayed owned starts are ignored and owned starts are not also sent to raw `on_event`. Ownership is inferred from the harness-routed tool name because `tool.started` currently has no provider/extension owner field.

Shell results include `success`, `status`, `signal`, `timed_out`, `duration_seconds`, `termination_reason`, `output`, `truncated`, optional `total_lines`, optional `total_bytes`, and `valid_utf8`. Default timeout is 120 seconds, maximum timeout is 86400 seconds, and each extension may have at most 32 pending shell jobs. Pending shell jobs are canceled when the extension connection shuts down.
Other host functions are available after `init` succeeds:

- `shell_spawn(command, opts)` executes a trusted host shell command asynchronously and returns a `ShellJob`. `opts` supports `timeout`, `cwd`, `on_complete`, and `tag`. Completion callbacks receive `(result, job)`. A tool handler returning `ShellJob` defers the tool result until the shell finishes; callback return values become `tool.result`, callback throws become `tool.error`, and no callback returns the full shell result map.
- `tau_emit(event)` emits a durable Tau event map.
- `tau_emit_transient(event)` emits a transient Tau event map.
- `tau_info(message)` and `tau_info(message, level)` emit transient `harness.notice`; `level` is `info`, `warning`, `debug`, or `trace` (`important` is accepted as a legacy spelling for `warning`).
- `tau_log(level, message)` writes only to extension logs.


## Safety and limitations

`std-rhai` runs trusted local scripts. Scripts can expose tools to agents and execute host shell commands through `shell_spawn`; shell execution is direct in `tau-ext-rhai` and intentionally does not use `tau-ext-shell` or its directory locks. Execution limits include `limits.max_operations`, `limits.max_call_levels`, and `limits.max_expr_depth`.

Event conversion supports the JSON-compatible subset of Tau payloads. Arbitrary CBOR bytes, tags, and non-string map keys are not faithfully represented yet.
