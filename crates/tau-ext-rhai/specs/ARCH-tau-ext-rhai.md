# ARCH-tau-ext-rhai: tau-ext-rhai architecture

`tau-ext-rhai` keeps Tau protocol framing in Rust and exposes JSON-shaped values to a trusted local Rhai script.

## Init and registration

`init(config)` is a staging phase. `register_tool_group` and `register_tool` are available only while init is active; other side-effecting host functions, including `shell_spawn`, are rejected. If init fails, the extension emits `ConfigError`, registers nothing, and terminates startup without `Ready`.

Tool and group names are validated with Tau protocol newtypes. Tool groups use empty specs in v1; a tool referencing an undeclared group gets an empty group attached to that tool registration.

## Runtime loop

The main Rhai interpreter stays single-threaded. The extension uses
`tau-client`'s deferred manual startup mode so it can send `Hello`, wait for the
initial `Configure`, run trusted `init(config)`, and only then emit the
script-computed `Subscribe`, `Intercept`, tool registrations, and terminal
`Ready`. If configuration or init fails, the deferred startup path emits one
`ConfigError` and terminates without `Ready`.

After startup, `tau-client` owns protocol decoding and serialized writing while
the Rhai runtime owns the policy loop. The loop is reactive rather than
poll-based: each iteration handles at most one ready harness input from
`ManualExtensionRuntime`, drains shell worker completions from a crate-local
channel, and then blocks on a coalesced tau-client wake primitive when neither
source can make progress. This keeps script execution non-concurrent while still
allowing host shell commands to run without blocking harness frame handling,
starving completion callbacks behind harness bursts, or introducing idle wakeups
and fixed completion latency.

## Tool dispatch

Live, non-replayed `tool.started` events whose tool name matches a registered Rhai tool are consumed by the tool dispatcher and not forwarded to raw `on_event`. Replayed owned starts are ignored. Current `ToolStarted` events do not carry provider/extension owner identity, so ownership is inferred from the harness-routed tool name; duplicate provider tool names are unsupported until the protocol grows an owner field or the harness enforces a stronger invariant.

## Shell execution

`shell_spawn` is direct trusted host execution in this extension, not `tau-ext-shell`. It does not participate in ext-shell directory locks. Pending shell jobs are capped per extension and timeouts are bounded before worker spawn. On Unix, commands run in their own process group; timeout and extension shutdown cancellation kill the group before collecting bounded stdout/stderr output.

Output capture never requires pipe EOF after the foreground shell has exited, timed out, or been canceled. A command can deliberately detach descendants into a different process group/session while leaving stdout/stderr inherited; those descendants may survive the owned process-group kill, so the extension performs only a bounded post-stop drain of immediately available pipe output before returning.
