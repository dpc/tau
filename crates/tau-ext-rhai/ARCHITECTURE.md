# tau-ext-rhai architecture

`tau-ext-rhai` keeps Tau protocol framing in Rust and exposes JSON-shaped values to a trusted local Rhai script.

## Init and registration

`init(config)` is a staging phase. `register_tool_group` and `register_tool` are available only while init is active; other side-effecting host functions, including `shell_spawn`, are rejected. If init fails, the extension emits `ConfigError`, registers nothing, and then sends an inert `Ready`.

Tool and group names are validated with Tau protocol newtypes. Tool groups use empty specs in v1; a tool referencing an undeclared group gets an empty group attached to that tool registration.

## Runtime loop

The main Rhai interpreter stays single-threaded. A reader thread converts harness frames into an internal queue, shell worker threads send completions to the same queue, and a writer thread serializes outbound harness messages. This lets shell jobs run without blocking harness message handling.

## Tool dispatch

Live, non-replayed `tool.started` events whose tool name matches a registered Rhai tool are consumed by the tool dispatcher and not forwarded to raw `on_event`. Replayed owned starts are ignored. Current `ToolStarted` events do not carry provider/extension owner identity, so ownership is inferred from the harness-routed tool name; duplicate provider tool names are unsupported until the protocol grows an owner field or the harness enforces a stronger invariant.

## Shell execution

`shell_spawn` is direct trusted host execution in this extension, not `tau-ext-shell`. It does not participate in ext-shell directory locks. Pending shell jobs are capped per extension and timeouts are bounded before worker spawn. On Unix, commands run in their own process group; timeout and extension shutdown cancellation kill the group before collecting bounded stdout/stderr output.
