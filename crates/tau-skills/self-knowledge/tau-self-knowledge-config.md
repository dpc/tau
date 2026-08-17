---
name: tau-self-knowledge-config
description: >
  Use this skill when the user asks how to configure Tau, where Tau stores config,
  state, sessions, runtime files, policies, credentials, or provider setup, or how
  to use tau init and tau provider commands.
advertise: false
---

# Tau configuration

## Enabling extensions from services and containers

Set `TAU_ENABLE_EXTENSIONS=NAME[,NAME...]`, for example
`TAU_ENABLE_EXTENSIONS=std-pim,std-xmpp`. Names are exact and case-sensitive.
ASCII spaces or tabs around names are allowed; empty elements, other whitespace,
punctuation, non-UTF-8 values, and unknown names fail startup. Duplicate names
are harmless. These enables apply after config and before ordered CLI extension
overrides, so a later `--disable-extension NAME` wins.
Unset, empty, and space/tab-only values are no-ops. Leading, trailing, and
consecutive commas are errors. `tau attach` and the outer `tau dev tmux` helper
reject nonempty values because they cannot apply them to their target harness.

Use the same literal with systemd
`Environment=TAU_ENABLE_EXTENSIONS=std-pim,std-xmpp`, Nix service environment
attributes, or a container `--env`. Environment keys do not append when repeated,
so construct one comma-separated value. The variable carries names only, not
arguments, shell syntax, secrets, or extension configuration.

Tau follows the XDG directory layout on Linux:

- Config: `~/.config/tau/`
  - `cli.yaml`, `cli.d/*.yaml` — CLI display preferences, key bindings, and prompt completions. See `tau-self-knowledge-cli-ui` for UI-specific behavior.
  - `harness.yaml`, `harness.d/*.yaml` — harness roles/defaults, extensions, tools, custom prompts, whole-session retention, and `diagnostic_retention_days` (14 by default; `0` disables startup cleanup of expired session `events.jsonl` and provider request/response diagnostics).
  - `testing.yaml` — explicit provider-profile allowlist for `tau dev tmux` E2E testing; see `tau-self-knowledge-e2e-testing`.
- State: `~/.local/state/tau/` or the platform/user state directory.
  - `sessions/<session_id>/` — durable session membership, metadata, logs, and debug captures.
  - `agents/<agent_id>/` — durable agent transcripts and metadata.
  - `cli.json` — persisted CLI runtime toggles.
  - `providers/<extension>/<provider>.json` — mutable credential-free provider settings.
  - `secrets/ext/<extension>/providers/<provider>/` — typed provider credentials.
- Runtime: `${XDG_RUNTIME_DIR}/tau/harnesses/` or `/tmp/tau-$USER/harnesses/`.
  - `<pid>-<instance>.sock` — daemon socket.
  - `<pid>-<instance>.json` — discovery metadata with pid, project root, version, and the
    daemon's current active `session_id` (updated after successful `:session new`).

Use `tau init` to create starter `cli.yaml` and `harness.yaml` files.

## Configuration profiles

`harness.yaml` can define named `profiles` patches for roles and supported
extension settings. `tau --profile work,review` applies the normal built-in and
user base configuration first, then `profiles.work`, then `profiles.review`.
Later profiles use the ordinary config merge rules, so a later scalar replaces
an earlier one and nested extension config maps merge recursively.

`TAU_PROFILE=work,review` selects the same ordered stack when `--profile` is
absent. ASCII spaces and tabs around names are ignored; empty, whitespace-only,
and unknown items fail startup. Tau preserves duplicates, so
`--profile work,work` applies the `work` patch twice. Without either selector,
`default_profile: work`
selects one base-configured fallback profile. Its surrounding ASCII spaces/tabs
are ignored, but it does not accept list syntax.

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

Agent-global, role-group, and role `required_skills` (camelCase alias
`requiredSkills`) list exact skill names that must be discoverable and
model-loadable before a role is available. Agent, group, and role requirements
are additive and de-duplicated. Missing,
hidden, or unreadable required skills emit a mandatory `harness.config_error`
notice and disable that role; if the startup/default role is disabled this way,
startup fails instead of silently falling back.

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
with `cli.yaml` `notice_level: warning` or runtime `:set notice-level warning`.

Per-secret `optional: true` is narrower: it omits only that secret when absent.
A missing non-optional secret skips the whole extension only when
`extensions.<name>.require: false`; otherwise it remains fatal.

Multiple instances of one tool extension can set a distinct `tool_prefix`, for
example `tool_prefix: work`. Tau then prefixes the structural internal tool name,
model-visible alias, and group (`work_slack_send` and group `work_slack`) while
leaving semantic tags and arbitrary descriptions/schemas unchanged. Exact
tool/group role policy uses the final names; tag policy still spans instances.
The setting is unrelated to the argv-wrapper `prefix`, and changing it requires
an extension restart. With no `tool_prefix`, names are unchanged.

## Agent IDs and display names

Tau mints durable agent IDs from the harness setting `agents.id_template`. Tau can also name newly created agents with optional `agents.display_name_template`:

```yaml
agents:
  id_template: "{{{{role}}}}-{{{{random_alphanumeric 4}}}}"
  display_name_template: "{{{{role_group}}}}: {{{{task_name}}}}"
```

The built-in ID template is `{{{{random_alphanumeric 6}}}}`; the built-in
display-name template is
`{{{{#if task_name_present}}}}{{{{task_name}}}}{{{{/if}}}}`. Thus a manually
created agent has no initial display name, while an explicit task name is kept
without adding its role. Both template types are rendered with Handlebars in
strict mode.

ID templates receive:

- `role` — the role name for the new agent.
- `role_group` — the name of the first configured role group containing the role, or the role name for ungrouped roles. `roleGroup` is also available as a camelCase alias.
- `random_alphanumeric <len>` — helper that emits an ASCII alphanumeric random suffix of at least `<len>` characters.

Display-name templates additionally receive:

- `agent_id` — the durable agent ID. `agentId` is also available as a camelCase alias.
- `task_name` — the requested task/display name for delegated or extension-started agents, or `""` when absent. `taskName` is also available as a camelCase alias.
- `task_name_present` — true when `task_name` is available. `taskNamePresent` is also available as a camelCase alias.
Rendered IDs must use only ASCII letters, digits, `_`, or `-`, and must fit Tau's agent ID length limit. If a configured ID template fails to render, renders an invalid ID, or keeps colliding, Tau warns and falls back to the built-in random template. If a configured display-name template fails to render or renders empty, Tau warns when appropriate and falls back to the request display name when one exists.

Delegated children started through the built-in `agent_start` tool use the exact
task title as their display name. Parent relationships remain separate metadata
and are not embedded in names. `:name` and `:agent name` set an explicit durable
display name even when it equals the agent's role. Persisted display-name facts
remain authoritative on replay, including role-derived defaults written by
older Tau versions; Tau does not guess whether such an old value was explicit.

## Providers

Use `tau provider add` for the interactive provider setup wizard. It prompts for provider kind, provider namespace, auth, and model details as needed.

Credential-free profiles may instead live under
`$XDG_CONFIG_HOME/tau/providers/<extension>/<provider>.json`. Config and state
profiles form a disjoint union, so duplicate names fail startup rather than
overriding. `tau provider add --config --output - [KIND]` emits canonical JSON
for dotfiles; credential records remain in state.

Other provider commands:

- `tau provider login <name>` — hydrate or refresh only the host-local Secret for
  an existing profile, preserving its settings source and bytes.
- `tau provider list [--config|--state|--all]` — show providers and sources, with
  an actionable login command for absent or expired ChatGPT authentication.
- `tau provider show <name>` — show credential-free JSON and its source path.
- `tau provider remove [--config|--state] <name>` — remove a provider profile.

Put `--extension <instance>` before the subcommand when targeting a renamed
built-in provider instance, for example
`tau provider --extension provider-work login chatgpt`. Bare ChatGPT add detects
a config-owned default profile that needs authentication and offers login instead
of creating a state duplicate.

Models are published by provider extensions at runtime; start Tau and use `:model` to inspect the current model list.

Provider cache refresh is a global, disabled-by-default harness policy:

```yaml
provider_cache_refresh:
  enabled: true
  max_idle_seconds: 300
```

The idle bound accepts 1 through 86,400 seconds. Eligible refreshes require an
exact safe sliding cache contract, explicit prices, and measured write/read
evidence. They resend the complete previous Provider prefix, including user/tool
context, only during bounded waits. Real prompts preempt refresh work, and no
refresh state persists across restart.

- `harness.yaml` can define `custom_prompts` as a map from prompt id to prompt text; in the CLI, `:prompt <id>` replaces the editable prompt buffer with that text without submitting it.
