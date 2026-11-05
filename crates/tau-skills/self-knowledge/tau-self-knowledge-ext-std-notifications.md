---
name: tau-self-knowledge-ext-std-notifications
description: Use when configuring or troubleshooting Tau std-notifications, including prompt/response sounds, idle notifications, OSC 1337 user vars, terminal bells, idle summaries, or notification commands.
advertise: false
---

# Notifications

`std-notifications` runs `tau-ext-std-notifications`. The extension is enabled
by default, but no notification hooks are configured.


## Features

OSC 1337 hooks can emit:

- `agent_start`, on user prompt submit: `user-notification = protoss-probe-ack`.
- `agent_end`, on final provider response when no tool call is requested and no background tools for that triggering agent remain: `user-notification = protoss-upgrade-complete`.
- `agent_idle`, after an idle window following a final response for that agent, whatever `user-text-notification` payload the user configured.
- `agent_idle_all`, after an idle window once every loaded agent in the session is idle.

`agent_summary: true` requests a one-sentence side-query summary from bounded
recent prompt/response text; Tau clamps long results before rendering.
Command hooks are detached, trusted local argv arrays; keep them short-lived.

Hook items can also emit `term.bell` with `bell: true`.

Hooks track each agent independently and ignore replayed history.
`agent.prompt_terminated` clears the prompt without completion/idle notifications;
later stale completions do not trigger hooks.


## Configuration

Configured under `extensions.std-notifications.config`. Configuration keys use
snake_case; kebab-case keys are rejected as typos.

```json5
extensions: {
  "std-notifications": {
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
          osc1337: { key: "user-text-notification", value: "...template..." },
        },
      ],
      "agent_idle_all": [],
    },
  },
}
```

Example `harness.yaml` idle hook that asks downstream terminal tooling to speak a short text-to-speech notification:

```yaml
extensions:
  std-notifications:
    enable: true
    config:
      agent_idle:
        - delay_seconds: 5
          agent_summary: false
          osc1337:
            key: user-tts-notification
            value: 'The agent {{agent.id}} on {{host}} in {{cwd_basename}} is waiting.'
```

Each hook item must set at least one of `bell`, `command`, or `osc1337`. The `command`, `osc1337.key`, and `osc1337.value` fields are Handlebars templates. Use the `json` helper when embedding untrusted values into JSON payloads, e.g. `"body":{{json turn.agent_summary}}`.

OSC keys must be nonempty printable ASCII, at most 128 bytes, with no `=`,
BEL/ESC, or controls. Static invalid keys reject config; runtime-invalid keys
are skipped and logged. Tau skips OSC values over 64 KiB and command argv
elements over 16 KiB.

Template variables:

- `hook` — hook currently firing: `agent_start`, `agent_end`, `agent_idle`, or `agent_idle_all`.
- `agent.id` — durable Tau agent id for the triggering conversation/agent.
- `agent.name` — current display name for the agent, falling back to `agent.id` when unset.
- `host` — hostname observed by the extension process.
- `cwd` — current working directory observed by the extension process.
- `cwd_basename` — final path component of `cwd`.
- `turn.user_prompt` — last user prompt text accepted into the agent transcript.
- `turn.agent_response` — final assistant text from the last completed provider response.
- `turn.agent_summary` — one-sentence side-query summary for idle hooks with `agent_summary: true`; empty on timeout or error.

Downstream terminal or desktop tooling is responsible for turning OSC user-var changes into audible or visual notifications.
