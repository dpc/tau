# tau-ext-std-notifications

A tau extension that bridges agent activity into iTerm2-style
[OSC 1337 `SetUserVar`][osc1337] user-variable updates. Mirrors the
shape of dpc's Pi extensions
[`notification-sounds.ts`][pi-sounds] and
[`idle-notification.ts`][pi-idle], adapted to tau's harness-mediated
event bus.

The extension itself does not play sounds or pop desktop notifications. It
emits configured terminal-facing side effects — OSC 1337 user-vars, terminal
bells, and detached commands. Downstream tooling is what turns those side
effects into audible or visual notifications.

## What it does

Tau's built-in configuration enables no notifications by default. When
configured, the four hook groups map to these trigger points:

| Hook | Trigger | Typical OSC user-var | Typical value |
|---|---|---|---|
| `agent_start` | `agent.prompt_submitted` (originator: User) | `user-notification` | `protoss-probe-ack` |
| `agent_end` | Final `provider.response_finished` (no pending tool calls, no active background tools for that agent, originator: User) | `user-notification` | `protoss-upgrade-complete` |
| `agent_idle` | Idle window elapses after a final response | `user-text-notification` | JSON payload (see below) |
| `agent_idle_all` | Idle window elapses after every loaded agent in a session is idle | `user-text-notification` | JSON payload (see below) |

The "final response" filter only treats responses with `tool_calls`
empty as the end of an agent turn. Mid-turn finishes (tool-call
batches the harness will run, then re-prompt) are skipped so the
end-of-turn sound only fires once per real turn.

If a user-originated tool call is backgrounded, the `agent_end` and
per-agent `agent_idle` hooks are deferred until that agent's background tool
finishes. Background tools for another loaded agent do not block the current
agent's completion notification.

The `originator: User` filter ensures *side conversations* spawned
by other extensions (or by this one — see below) do not retrigger
the sounds or perturb the idle state machine.

## Idle text notification

After `delay_seconds` (default 60 when omitted on a configured idle hook) of
inactivity following a final agent response, the example `agent_idle` hook fires
the `user-text-notification` user-var with the static "Waiting for user
input" body. `agent_idle_all` uses the same hook item schema, but arms only
when a session transitions from at least one busy loaded agent to every loaded
agent being idle.

If an idle hook sets `agent_summary` to `true`, the extension first asks the
agent for a one-sentence summary before firing that hook:

1. The extension sends an `agent.start_request` to the harness asking for a
   one-sentence summary. The request instruction includes a bounded copy of the
   captured user prompt and assistant response so the side conversation has the
   turn context it is supposed to summarize.
2. The harness spawns a fresh side conversation, dispatches the explicit-context
   prompt to the agent, and routes the matching `agent.start_result` back
   point-to-point. Parent linkage, when used by the harness, is only for
   metadata/session inheritance; summary correctness must come from the prompt
   context above.
3. The extension exposes the result as `turn.agent_summary` while rendering the
   configured hook templates. Long summaries are clamped before rendering. On
   timeout or error, `turn.agent_summary` is an empty string. The configured
   template decides how, or whether, to include it.

The idle deadline resets on:

- `agent.prompt_submitted` (originator: User) — the user prompt was
  accepted into the agent transcript;
- `provider.prompt_submitted` (originator: User), when its
  `agent_prompt_id` maps to the same agent — the provider is starting a real
  turn; unowned provider prompts leave existing per-agent idle deadlines
  unchanged so one agent cannot suppress another agent's idle notification;
- `ui.prompt_draft` — trailing-edge debounced typing pings from
  the UI; the deadline jumps back by that idle hook's `delay_seconds`
  so the notification doesn't fire mid-sentence while the user is
  composing. This extension intentionally treats prompt drafts only as
  liveness/idle-reset signals and ignores their `target_agent_id`; it does not
  restore, synchronize, or inspect draft contents. Only applies in the
  `WaitingIdle` state; an
  in-flight side-query summary (`WaitingSummary`, only possible when
  `agent_summary` is enabled) is left alone because we don't
  currently have a way to cancel the agent's in-flight prompt without
  billing for it.

## Text notification payloads

The extension does not impose a text-notification schema. If you want to drive
`user-text-notification.sh` or another downstream consumer, configure the
`osc1337.key` and `osc1337.value` templates to whatever payload that consumer
expects.

## Configuration

The extension reads its config from the `extensions.<name>.config`
field of `harness.yaml`. Hook/configuration keys are snake_case; do not add
kebab-case aliases for new keys. All fields are optional; unknown fields
are rejected with a `lifecycle.config_error` so the harness can
surface typos to the user.

```json5
{
  extensions: {
    "std-notifications": {
      enable: true,
      config: {
        "agent_start": [
          { osc1337: { key: "user-notification", value: "protoss-probe-ack" } },
        ],
        "agent_end": [
          { osc1337: { key: "user-notification", value: "protoss-upgrade-complete" } },
        ],
        "agent_idle": [
          {
            delay_seconds: 60,
            agent_summary: false,
            osc1337: {
              key: "user-text-notification",
              value: "{\"title\":\"Agent idle: {{host}}:{{cwd_basename}}\",\"body\":\"Waiting for user input\"}",
            },
          },
        ],
        "agent_idle_all": [],
      },
    },
  },
}
```

Each hook is an array, so a single trigger can emit multiple side
effects. Each item must set at least one action and supports:

- `bell: true` to emit `term.bell`;
- `command: ["program", "arg template", ...]` to spawn a detached command;
- `osc1337: { key, value }` to emit `term.osc1337_set_user_var`.

The `command`, `key`, and `value` strings are Handlebars templates in
strict mode. Current variables include `hook`, `agent.id`, `agent.name`
(the durable display name, falling back to the id), `host`, `cwd`,
`cwd_basename`, `turn.user_prompt`, `turn.agent_response`, and
`turn.agent_summary` (set only for idle hooks with
`agent_summary: true`, empty on timeout/error).

Use the `json` helper when embedding untrusted template values into a JSON
payload, for example `"body":{{json turn.agent_summary}}`. The helper renders a
JSON literal string/value; do not wrap its output in additional quotes.

Rendered OSC user-var keys are validated before emission, and the terminal UI
validates again before writing escape sequences. Keys must be non-empty
printable ASCII, must not contain `=`, BEL/ESC, or other control characters, and
must be at most 128 bytes. A statically invalid key rejects the config; a key
that only becomes invalid after rendering untrusted runtime data is skipped and
logged.

Rendered OSC values larger than 64 KiB are skipped so untrusted prompt,
response, or summary text cannot amplify terminal-facing side effects without
bound.

Command hooks are trusted local configuration. The extension spawns them
detached from stdin/stdout/stderr, but each command runs in its own thread until
the process exits. Keep commands short-lived and avoid hooks that can block
indefinitely. Rendered command argv elements larger than 16 KiB are skipped.

### Bell-only completion example

```json5
{
  extensions: {
    "std-notifications": {
      config: {
        "agent_start": [],
        "agent_end": [{ bell: true }],
        "agent_idle": [],
        "agent_idle_all": [],
      },
    },
  },
}
```

## Tracing

The extension uses the `std-notifications` tracing target:

```sh
TAU_LOG=std-notifications=debug tau …
```

`debug` shows `received StartAgentResult { idle_hooks, query_id,
text_len, error }` and idle-deadline transitions; `trace` adds one
line per ignored event for protocol-level debugging.

[osc1337]: https://iterm2.com/documentation-escape-codes.html
[pi-sounds]: https://github.com/dpc/dpc-personal/blob/master/.pi/agent/extensions/notification-sounds.ts
[pi-idle]: https://github.com/dpc/dpc-personal/blob/master/.pi/agent/extensions/idle-notification.ts
[var]: ../tau-proto/src/events.rs
