---
name: tau-self-knowledge-ext-std-notifications
description: Use this extension skill when the user asks about Tau's std-notifications extension, prompt/response sounds, idle notifications, OSC 1337 user vars, terminal bells, idle summaries, or notification commands.
advertise: false
---

# Tau std-notifications extension self-knowledge

`std-notifications` is Tau's built-in notification extension. It runs `tau-ext-std-notifications`, is enabled by default, and reacts to prompt/response/idle events by emitting terminal-facing notification events.


## Features

Tau's built-in configuration enables no notifications by default. A typical OSC 1337 hook configuration emits:

- `agent_start`, on user prompt submit: `user-notification = protoss-probe-ack`.
- `agent_end`, on final provider response when no tool call is requested and no main-agent background tools remain: `user-notification = protoss-upgrade-complete`.
- `agent_idle`, after an idle window following a final response, whatever `user-text-notification` payload the user configured.
- `agent_idle_all`, after an idle window once every loaded agent in the session is idle.

If an idle hook's `agent_summary` is true, the idle path first asks the agent for a one-sentence summary before firing that hook. The side-query instruction includes a bounded copy of the recent user prompt and assistant response so the summary has explicit turn context, and long summary results are clamped before template rendering. Hook commands are detached argv arrays rendered as Handlebars templates; they are trusted local configuration and should be short-lived.

Hook items can also emit `term.bell` with `bell: true`.

The extension reacts only to live events. Replay-marked frames (subscribe-time catch-up history the harness re-delivers when an extension joins an already-initialized session) are skipped, so old prompts and responses never ring sounds or fire idle notifications.


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

Each hook item must set at least one of `bell`, `command`, or `osc1337`. The `command`, `osc1337.key`, and `osc1337.value` fields are Handlebars templates.

Rendered OSC user-var keys must be non-empty printable ASCII, must not contain `=`, BEL/ESC, or other control characters, and must be at most 128 bytes. Statically invalid keys reject the config; keys that become invalid only after runtime template rendering are skipped and logged.

Template variables:

- `hook` — hook currently firing: `agent_start`, `agent_end`, `agent_idle`, or `agent_idle_all`.
- `agent.id` — durable Tau agent id for the main conversation.
- `agent.name` — current display name for the agent, falling back to `agent.id` when unset.
- `host` — hostname observed by the extension process.
- `cwd` — current working directory observed by the extension process.
- `cwd_basename` — final path component of `cwd`.
- `turn.user_prompt` — last user prompt text accepted into the agent transcript.
- `turn.agent_response` — final assistant text from the last completed provider response.
- `turn.agent_summary` — one-sentence side-query summary for idle hooks with `agent_summary: true`; empty on timeout or error.

Downstream terminal or desktop tooling is responsible for turning OSC user-var changes into audible or visual notifications.
