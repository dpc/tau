# tau-ext-rhai

Prototype trusted local scripting extension for Tau. The built-in extension is disabled by default; enable it with a script path:

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

`script` is required. Relative paths are resolved by the extension process current working directory; absolute paths are preferred. `vars` is passed to `init(config)` and `start(config)` as JSON-compatible data, along with `state_dir` when the harness provides one.

Scripts see Tau events as JSON-shaped maps matching Serde's JSON form. Use
`tau_info("hi")`, not a fabricated `harness.notice` event, for a user-visible
diagnostic:

```rhai
#{ event: "term.bell", payload: #{} }
```

## Callbacks

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

`init(config)` is optional. A missing `init` or a no-op/unit return means no subscriptions, no intercepts, and the default ready message. `subscribe` accepts selector maps shaped as `#{ kind: "exact", value: "agent.prompt_submitted" }` or `#{ kind: "prefix", value: "tool." }`. Prefer exact subscription selectors that list the events the script actually handles; prefix subscriptions are only for scripts intentionally observing a whole event category and can increase replay catch-up, traffic, and data exposure. Multiple `intercept` entries are merged only when they use the same `priority`; different priorities are rejected because the harness has one interceptor registration per connection.

`start(config)` is optional and runs once after `init` succeeds, subscriptions/intercepts are sent, `Ready` is sent, and host functions are registered. Use it for startup side effects such as `tau_info`; callback errors request warning diagnostics without disabling the extension.

`on_event(event, meta)` is optional. `meta.replay` is `true` when the delivery is subscribe-time catch-up history rather than a live occurrence; `meta.recorded_at` carries the original commit timestamp when Tau supplies it. Scripts with user-visible side effects should skip replayed events.

`on_intercept(event, persist)` is optional. Return values are:

- `()` or `"pass"` or `#{ kind: "pass" }` — pass the original event.
- `#{ kind: "pass", event: event }` — pass a replacement event.
- `"drop"` or `#{ kind: "drop" }` — drop the event.

## Host functions

- `register_tool_group(name, spec)` — during `init` only, stage a tool group. Group names use Tau's validated tool-group identifier syntax.
- `register_tool(name, spec, handler)` — during `init` only, stage an agent-invokable tool. `handler` is a Rhai function pointer such as `Fn("project_status")`; Tau calls it as `handler(args, call_info)` for live owned `tool.started` events. The `spec` map supports `description`, `parameters`, `model_visible_name`, `enabled_by_default`, and `group`.
- `shell_spawn(command, opts)` — start a trusted host shell command asynchronously and return a `ShellJob`. `opts` supports `timeout`, `cwd`, `on_complete`, and `tag`. The completion callback is called as `on_complete(result, job)`.
- `tau_emit(event)` — emit a durable Tau event map.
- `tau_emit_transient(event)` — emit a transient Tau event map.
- `tau_info(message)` / `tau_info(message, level)` — request a live user notice; the harness owns and sanitizes the resulting `harness.notice`. `level` is `"info"`, `"warning"`, `"debug"`, or `"trace"` (`"important"` is accepted as legacy `"warning"`).
- `tau_log(level, message)` — write private script-authored text to the
  `rhai-script-private` debug target. Enable it explicitly with
  `TAU_LOG='rhai-script-private=debug,warn'`.

`register_tool*` are available only during `init`. Other side-effecting host functions are available to `start`, raw event/intercept callbacks, tool handlers, and shell completion callbacks, but not during `init`. This keeps broken init scripts inert.

Tool handlers complete the call with their normal return value, fail it if they throw, or defer completion by returning a `ShellJob`. If a deferred shell job has an `on_complete` callback, that callback's return value becomes the tool result and a thrown callback becomes `tool.error`; without a callback, the full shell result map is returned.

`call_info` contains `call_id`, `tool_name`, `agent_id`, `originator`, and `tool_type`. `register_tool_group` requires an empty spec map in v1. If a tool references an undeclared group, the registration auto-creates an empty group for that tool. `register_tool` does not expose Tau tool tags yet; Rhai tools are currently registered without tag metadata.

Shell results include `success`, `status`, `signal`, `timed_out`, `duration_seconds`, `termination_reason`, `output`, `truncated`, optional `total_lines`, optional `total_bytes`, and `valid_utf8`. The default timeout is 120 seconds, the maximum timeout is 86400 seconds, at most 32 shell jobs may be pending per extension, stdout is captured before stderr with stderr appended under `[stderr]`, and captured output is capped/truncated. Shell commands run directly in `tau-ext-rhai`; on Unix each command gets its own process group so timeout and extension shutdown can kill ordinary descendants. Commands can still detach descendants into another process group/session while keeping stdout/stderr inherited, so output capture after foreground completion, timeout, or cancellation uses a bounded post-stop drain and does not wait indefinitely for pipe EOF.

Owned `tool.started` dispatch is name-based because the current Tau event carries no provider/extension owner identity; this relies on the harness-routed visible tool name being the available ownership signal.

Example:

```rhai
fn init(config) {
    register_tool_group("host", #{});
    register_tool("project_status", #{
        group: "host",
        description: "Get project status",
        parameters: #{ type: "object", additionalProperties: false },
    }, Fn("project_status"));
}

fn project_status(args, call_info) {
    return shell_spawn("git status --short", #{
        timeout: 30,
        on_complete: Fn("project_status_done"),
    });
}

fn project_status_done(result, job) {
    if !result.success {
        throw `git status failed: ${result.output}`;
    }
    return result.output;
}
```

## Limitations

Scripts are trusted local code. Rhai can expose agent-invokable tools and can execute host shell commands through `shell_spawn`; do not run scripts you would not run as local code. Shell execution is direct in `tau-ext-rhai` and intentionally does not use ext-shell locking. Event conversion supports the JSON-compatible subset of Tau payloads; arbitrary CBOR bytes, tags, and non-string map keys are not faithfully represented yet.
