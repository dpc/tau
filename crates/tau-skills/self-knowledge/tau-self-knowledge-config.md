---
name: tau-self-knowledge-config
description: >
  Use this skill when the user asks how to configure Tau, where Tau stores config,
  state, sessions, runtime files, policies, credentials, or provider setup, or how
  to use tau init and tau provider commands.
advertise: false
---

# Tau configuration

Tau follows the XDG directory layout on Linux:

- Config: `~/.config/tau/`
  - `cli.yaml`, `cli.d/*.yaml` — CLI display preferences, key bindings, and prompt completions. See `tau-self-knowledge-cli-ui` for UI-specific behavior.
  - `harness.yaml`, `harness.d/*.yaml` — harness roles/defaults, extensions, tools, custom prompts, and session retention.
- State: `~/.local/state/tau/` or the platform/user state directory.
  - `sessions/<session_id>/` — durable session membership, metadata, logs, and debug captures.
  - `agents/<agent_id>/` — durable agent transcripts and metadata.
  - `cli.json` — persisted CLI runtime toggles.
  - `policy.cbor` — persisted socket-client policy decisions.
  - `auth.d/<provider>.json` — provider credentials; `auth.json` may exist as legacy credentials.
- Runtime: `${XDG_RUNTIME_DIR}/tau/<pid>/` or `/tmp/tau-$USER/<pid>/`.
  - `tau.sock`, `tau.pid`, `tau.session_id`, `tau.dir` — daemon socket and discovery markers.

Use `tau init` to create starter `cli.yaml` and `harness.yaml` files.

## Built-in defaults

Tau layers these defaults underneath user config and `*.d/*.yaml` drop-ins.

### Harness defaults

```yaml
{harness_config}
```

`tool_policy.rules` is harness-owned declarative tool-surface policy. Rules are
keyed so a user can disable built-ins such as `builtin.chatgpt-shell` with
`enable: false`; matching rules run `disable_tool_tags` before
`enable_tool_tags`. Rules sort by `priority` (default `0`, lower first) and then
rule name. Tag patterns are exact or terminal-prefix forms like `shell:*`. Rule
names may contain dots, so CLI overrides should use a whole-map value, for
example `tool_policy={{{{rules: {{{{builtin.chatgpt-shell: {{{{enable: false}}}}}}}}}}}}`.

### CLI UI defaults

```yaml
{ui_config}
```

## Extension availability

Harness extensions are configured under `extensions.<name>` in `harness.yaml`.
Use `enable: false` to disable an extension entirely. Enabled extensions default
to `require: true`, which preserves startup-fatal behavior for harness-owned
startup failures such as an empty command, missing required declared secret, or
spawn failure. Set `require: false` next to `enable` when the extension is useful
but optional; Tau will skip it on startup/config/secret/pre-ready failures,
continue without it, and emit a mandatory warning `harness.notice` explaining
the skip. Harness notices have stable `kind` strings and levels `critical`,
`warning`, `info`, `debug`, and `trace`; CLI users can set the default threshold
with `cli.yaml` `notice_level: warning` or runtime `/set notice-level warning`.

Per-secret `optional: true` is narrower: it omits only that secret when absent.
A missing non-optional secret skips the whole extension only when
`extensions.<name>.require: false`; otherwise it remains fatal.

## Agent IDs and display names

Tau mints durable agent IDs from the harness setting `agents.id_template`. Tau can also name newly created agents with optional `agents.display_name_template`:

```yaml
agents:
  id_template: "{{{{role_group}}}}-{{{{random_alphanumeric 4}}}}"
  display_name_template: "{{{{role_group}}}}: {{{{task_name}}}}"
```

The built-in ID template is `{{{{random_alphanumeric 6}}}}`; the built-in display-name template is `{{{{#if task_name_present}}}}{{{{role}}}}: {{{{task_name}}}}{{{{else}}}}{{{{role}}}}{{{{/if}}}}`. Both template types are rendered with Handlebars in strict mode.

ID templates receive:

- `role` — the role name for the new agent.
- `role_group` — the name of the first configured role group containing the role, or the role name for ungrouped roles. `roleGroup` is also available as a camelCase alias.
- `random_alphanumeric <len>` — helper that emits an ASCII alphanumeric random suffix of at least `<len>` characters.

Display-name templates additionally receive:

- `agent_id` — the durable agent ID. `agentId` is also available as a camelCase alias.
- `task_name` — the requested task/display name for delegated or extension-started agents, or `""` when absent. `taskName` is also available as a camelCase alias.
- `task_name_present` — true when `task_name` is available. `taskNamePresent` is also available as a camelCase alias.
Rendered IDs must use only ASCII letters, digits, `_`, or `-`, and must fit Tau's agent ID length limit. If a configured ID template fails to render, renders an invalid ID, or keeps colliding, Tau warns and falls back to the built-in random template. If a configured display-name template fails to render or renders empty, Tau warns when appropriate and falls back to the request display name when one exists.

## Providers

Use `tau provider add` for the interactive provider setup wizard. It prompts for provider kind, provider namespace, auth, and model details as needed.

Other provider commands:

- `tau provider list` — show configured provider profiles.
- `tau provider remove <name>` — remove a provider profile.

Models are published by provider extensions at runtime; start Tau and use `/model` to inspect the current model list.

- `harness.yaml` can define `custom_prompts` as a map from prompt id to prompt text; in the CLI, `/prompt <id>` replaces the editable prompt buffer with that text without submitting it.
