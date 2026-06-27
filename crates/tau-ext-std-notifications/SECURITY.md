# tau-ext-std-notifications security notes

`std-notifications` bridges harness events into terminal-facing side effects.
It should treat harness event text and display names as untrusted template data.

## Trust boundaries

- Configuration is trusted local user configuration, but typos should fail
  closed through `ConfigError`.
- Template inputs (`agent.name`, prompts, responses, summaries, cwd, hostname)
  can contain arbitrary user/model text.
- The terminal UI validates OSC 1337 user-var names and skips invalid names as
  defense in depth before writing escape sequences. This crate also validates
  rendered names before emitting them so bad configuration fails closed early.

## OSC 1337 keys

Rendered `osc1337.key` values must be non-empty printable ASCII, must not
contain `=`, BEL/ESC, or other control characters, and must be at most 128
bytes. Statically invalid keys reject configuration. Keys that become invalid
only after rendering runtime data are skipped and logged.

`osc1337.value` is not restricted by this crate because the UI base64-encodes
the value before writing the terminal escape. When the UI runs inside tmux it
wraps the OSC sequence for tmux passthrough.

## Command hooks

Command hooks execute trusted local commands from user configuration. They run
with the extension process environment and current working directory, with
stdin/stdout/stderr detached. A hook command that blocks can keep its worker
thread and child process alive indefinitely, so configure only short-lived
commands.

## Summary side queries

Idle summary hooks start side-agent requests. The instruction includes a
bounded copy of the captured user prompt and assistant response, so summaries do
not rely on inherited transcript state. Summary result text is clamped before it
is exposed as `turn.agent_summary`. Summary failures or timeouts fall back to an
empty `turn.agent_summary` value and still allow the notification to fire.

## Failure behavior

Malformed configuration is reported as `ConfigError` and the previous config
remains in effect. Runtime-invalid OSC keys are skipped rather than emitted.
Hook template rendering failures are returned from the protocol loop because
they indicate accepted configuration no longer renders with the current event
context.
