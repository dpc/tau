---
name: tau-self-knowledge-ext-rhai
description: Use when configuring or developing Tau std-rhai scripts, including its disabled-by-default status, event hooks, script config, subscriptions, interceptions, host functions, or scripting limitations.
advertise: false
---

# Tau std-rhai extension self-knowledge

`std-rhai` runs trusted local Rhai scripts through `tau-ext-rhai`. It is disabled
by default. Scripts receive JSON-shaped event maps matching Serde's event form;
the extension handles protocol framing.

## Configuration

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

`script` is required; relative paths use the extension process cwd. Both `init`
and `start` receive JSON-compatible `vars` and the harness's `state_dir`, if
present. Config, read, compile, or `init` failure rejects startup before `Ready`.
Other execution limits are `limits.max_call_levels` and `limits.max_expr_depth`.

## Callbacks

All callbacks are optional:

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

fn on_intercept(event, persist) {
    event.payload.text = event.payload.text.replace("tao", "tau");
    return #{ kind: "pass", event: event };
}
```

- Missing or unit-returning `init` declares no subscriptions/intercepts. Prefer
  `exact` selectors; `prefix` subscriptions increase catch-up traffic and exposure.
  Multiple intercept entries must share one priority; Tau merges their selectors.
- `start` runs once after declarations and `Ready`, with host functions available.
  Errors warn without disabling the extension.
- `on_event` receives `meta.replay` and, when supplied, `meta.recorded_at`.
  Skip replayed events when performing external side effects.
- `on_intercept` returns `()` / `"pass"` / `#{ kind: "pass" }` for the original,
  `#{ kind: "pass", event: event }` for a replacement, or
  `"drop"` / `#{ kind: "drop" }`. Errors and invalid returns warn and pass the original.

## Host functions

During `init` only, use `register_tool_group(name, spec)` and
`register_tool(name, spec, handler)`. Group specs must be empty. Tool specs accept
`description`, `parameters`, `model_visible_name`, `enabled_by_default`, and
`group`, but no tags. Tau auto-creates undeclared groups.

Handlers such as `Fn("my_tool")` receive `(args, call_info)` with `call_id`,
`tool_name`, `agent_id`, `originator`, and `tool_type`. Owned tool starts do not
also reach `on_event`; replayed starts do not execute handlers.

After `init`:

Host-reported tool failures use `rhai-tool-failed` with diagnostic and recovery
text in the semantic `output` payload. The host leaves successful script values and generic script-authored emits
unchanged. Scripts are responsible for the conformance of events they construct.

- `shell_spawn(command, opts)` returns a `ShellJob`. Options: `timeout`, `cwd`,
  `on_complete`, `tag`. Completion callbacks receive `(result, job)`. Returning a
  job from a tool handler defers its result: the callback return becomes
  `tool.result`, a throw becomes `tool.error`, and no callback returns the shell map.
- `tau_emit(event)` emits a durable event; `tau_emit_transient(event)` emits a
  transient event, for example `#{ event: "term.bell", payload: #{} }`.
- `tau_info(message)` and `tau_info(message, level)` request sanitized live notices. Levels:
  `info`, `warning`, `debug`, `trace`; legacy `important` means `warning`.
  Use this instead of fabricating `harness.notice`.
- `tau_log(level, message)` writes private script text only when enabled with
  `TAU_LOG='rhai-script-private=debug,warn'`.

Shell results include `success`, `status`, `signal`, `timed_out`,
`duration_seconds`, `termination_reason`, `output`, `truncated`, optional
`total_lines`/`total_bytes`, and `valid_utf8`. Timeout defaults to 120 seconds,
maximum 86400; at most 32 jobs may be pending. Each pipe retains 512 KiB;
invalid/cap-cut UTF-8 becomes replacement characters with `valid_utf8: false`.
Disconnect cancels jobs. On Unix, timeout/shutdown kills ordinary descendants,
but detached descendants may survive; capture does not wait indefinitely for EOF.

## Limits

Scripts execute shell commands directly, bypassing `tau-ext-shell` and its
directory locks. Event conversion supports JSON-compatible payloads, not arbitrary
CBOR bytes, tags, or non-string map keys.
