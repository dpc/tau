# ARCH-tau-ext-std-notifications: tau-ext-std-notifications architecture

`std-notifications` is a user-facing side-effect bridge. It listens to harness
events and emits terminal-facing notification actions; it does not own agent
execution. Bell and OSC requests are live side effects under
[SPEC-terminal-output-side-effect-events](../../../specs/SPEC-terminal-output-side-effect-events.md).

The extension runs on `tau-client`'s manual-loop runtime. Tau-client owns
startup, subscriptions, configuration dispatch, live event delivery, and the
outbound writer. This crate owns the notification policy loop, including timer
deadlines. Clean input EOF stops event intake but allows pending idle hooks to
fire; explicit disconnect does not drain them.

## Turn and idle state

Prompt-start and turn-end notifications apply only to user-originated main
turns. Side conversations do not emit these hooks or perturb idle timers.
Visible-turn state is tracked per agent so interleaved agents cannot suppress
each other's hooks or mix template data.

Provider prompt-start events cancel idle state only through a known prompt-to-
agent mapping. Missing ownership never clears every agent's timer.
`agent.prompt_terminated` consumes that prompt's notification state without
emitting completion hooks. Background tool blockers remain until their matching
terminal background event so a terminated prompt cannot create a false
completion notification.

Per-agent idle timers follow completed user turns. All-agent idle timers are
session-scoped and use harness-owned loaded membership and `agent.state`
snapshots as their busy/idle authority; provider final responses only update
template context. Session shutdown removes that session's timers and membership.
`ui.prompt_draft` extends idle timers only before summary work starts, under
[SPEC-ui-prompt-draft-and-focus-events](../../../specs/SPEC-ui-prompt-draft-and-focus-events.md).

Optional idle summaries run as correlated side-agent requests. Their instructions
contain bounded copies of the triggering user prompt and assistant response
rather than relying on inherited transcript. Summary agents are excluded from
the all-idle state that caused them, and failures or timeouts yield an empty
summary without suppressing the notification. Their request/result boundary is
[SPEC-start-agent-requests](../../../specs/SPEC-start-agent-requests.md).

## Side-effect boundary

Configuration is trusted local input, while prompt, response, summary, agent
name, cwd, and hostname template values are untrusted text. Command hooks execute
trusted configured commands in the extension environment, so operators must use
short-lived commands. Rendered command arguments and OSC values are bounded
before emission.

OSC 1337 user-variable names must be non-empty printable ASCII, exclude `=` and
control characters, and remain within the bounded name length. The extension
rejects statically invalid configuration and skips names that become invalid at
runtime; the terminal UI repeats validation before writing escape sequences.
OSC values may contain arbitrary text because the UI base64-encodes them, but
oversized values are skipped. JSON templates must use the `json` helper for
untrusted values rather than manually quoting them.

Malformed configuration leaves the previous configuration active. Runtime
rendering failures return from the protocol loop because accepted configuration
must remain renderable with current event context.
