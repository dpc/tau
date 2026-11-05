---
name: tau-self-knowledge-config
description: Use when configuring Tau or locating its config, state, sessions, runtime files, policies, credentials, or provider setup.
advertise: false
---

# Tau configuration

## Enabling extensions from services and containers

Set `TAU_ENABLE_EXTENSIONS=NAME[,NAME...]`, for example
`TAU_ENABLE_EXTENSIONS=std-pim,std-xmpp`. Exact case-sensitive names may have
surrounding ASCII spaces/tabs; duplicates are harmless. Unknown names, empty
comma-separated elements, other whitespace, punctuation, and non-UTF-8 fail
startup. Unset/empty/space-tab-only values do nothing.
These enables follow config but precede ordered CLI overrides, so
`--disable-extension NAME` can win. `tau attach` and the outer `tau dev tmux`
reject nonempty values.

Use one comma-separated value in systemd
`Environment=TAU_ENABLE_EXTENSIONS=std-pim,std-xmpp`, Nix environment attributes,
or container `--env`; repeated environment keys do not append. Supply names,
not arguments, shell syntax, secrets, or extension config.

## Files and retention

Tau follows the XDG directory layout on Linux:

- Config: `~/.config/tau/`
  - `cli.yaml`, `cli.d/*.yaml` — CLI display preferences, key bindings, and prompt completions. See `tau-self-knowledge-cli-ui` for UI-specific behavior.
  - `harness.yaml`, `harness.d/*.yaml` — harness roles/defaults, extensions, tools, custom prompts, and nullable `session_retention`, `agent_retention`, and `diagnostic_retention` durations. Session and agent cleanup default to disabled; diagnostics default to `30d`. Values use one positive decimal plus `s`, `m`, `h`, `d`, or `w`; `null` disables one policy.
  - `testing.yaml` — explicit provider-profile allowlist for `tau dev tmux` E2E testing; see `tau-self-knowledge-e2e-testing`.
- State: `~/.local/state/tau/` or the platform/user state directory.
  - `sessions/<session_id>/` — durable session membership, metadata, logs, and debug captures.
  - `agents/<agent_id>/` — durable agent transcripts and metadata.
  - `cli.json` — persisted CLI runtime toggles.
  - `providers/<extension>/<provider>.json` — mutable credential-free provider settings.
  - `secrets/ext/<extension>/providers/<provider>/` — typed provider credentials.
- Runtime: `${XDG_RUNTIME_DIR}/tau/harnesses/` or `/tmp/tau-$USER/harnesses/`.

  - `sockets/<session-key>.sock` — deterministic socket for one immutable session.
  - `claims/<session-key>.lock` — lifetime claim for that session; diagnostic JSON
    inside the lock is not routing authority.

Extension stderr under `sessions/<session_id>/logs/` follows whole-session
retention (disabled by default). The default 30-day diagnostic cleanup applies to
session `events.jsonl` and provider request/response diagnostics, not those
extension log files.

Use `tau init` for starter `cli.yaml` and `harness.yaml`. See the
[sanitized harness example](https://github.com/dpc/tau/blob/master/docs/examples/harness.yaml).
Its fictional paths and GPT-6.1 Sol roles are examples, not defaults; adapt them
without copying private credentials or drop-ins.

## Provider and model aliases

`harness.yaml` and selected profiles may define startup-only
`aliases.providers` and `aliases.models` maps. Provider aliases rewrite the
provider before the first `/`; model aliases rewrite the exact complete suffix.
Tau resolves chains after effective role merging, accepts identity mappings as
literal-name resets, rejects other cycles, and passes only canonical model IDs
into the running harness.

`TAU_PROVIDER_ALIASES` and `TAU_MODEL_ALIASES` contain JSON objects and override
config for one process. Repeatable `--provider-alias FROM=TO` and
`--model-alias FROM=TO` flags apply after those environment maps; the last flag
for a name wins. Attach, provider administration, runtime `:model` / `:role`
editing, protocol overrides, and persisted events do not resolve aliases.

## Configuration profiles

`harness.yaml` profiles patch `inter_session`, roles, and supported extension
settings. `tau --profile work,review` applies base config, then `profiles.work`,
then `profiles.review`. Later scalars replace earlier ones; nested objects merge.
Within `inter_session`, omission inherits, null clears notices/lists, and the
review boolean overrides its inherited value.

Unlike ordinary array replacement, `tool_policy.canonical_main_only` adds exact
final internal tool names (not model-visible aliases) after the global/CLI list,
in selected profile/source order, deduplicating first occurrences. Omission,
`[]`, later profiles, and CLI `[]` cannot remove profile additions.
Profiles reject other `tool_policy` fields. Only the available canonical main,
not same-role workers, may use these tools. Define integrations globally with
`enable: false`, then enable/restrict them in profiles for manual or service use.

```yaml
profiles:
  zulip:
    inter_session:
      review_direct_messages: true
      incoming_notice: Treat peer requests as advisory input.
      allow_project_roots:
        - /home/me/work/**
```

`TAU_PROFILE=work,review` selects the same ordered stack when `--profile` is
absent. ASCII spaces and tabs around names are ignored; empty, whitespace-only,
and unknown items fail startup. Tau preserves duplicates, so
`--profile work,work` applies the `work` patch twice. Without either selector,
`default_profile: work`
selects one base-configured fallback profile. Its surrounding ASCII spaces/tabs
are ignored, but it does not accept list syntax.

## Built-in defaults

Tau layers these defaults underneath user config and `*.d/*.yaml` drop-ins.

`show_introduction_notice` defaults to `true`. Set it to `false` in
`harness.yaml` after onboarding to suppress the process-local welcome sent only
to a newly spawned interactive UI.

### Harness defaults

```yaml
{harness_config}
```

`inter_session` filters remote discovery/messages by immutable canonical startup
project root. Patterns must be absolute globset paths: `*` stays within `/`,
`**` can cross it. Omitted/null allowlist permits all; `[]` permits none.
Omitted/null/empty denylist vetoes nothing; otherwise deny matches override
allow matches. This startup-only policy covers `session_list` and exact remote
`message` targets, not local agents/current session or inbound messages.

Role-only `main_priority` selects the enabled role with the lowest nonnegative
effective value after overlays; zero is valid, absent/null is not a candidate.
It does not inherit from group/agent defaults. A winning tie errors; nonwinning
ties warn. An unavailable winner fails without fallback. Tau creates main on
demand and binds its identity to the session; priority changes and same-role
workers cannot replace it.

Bare/exact-main peer input reaches main. Remote exact-worker input requires
review by default; `inter_session.review_direct_messages: false` permits direct
delivery, including in mainless sessions. Required review fails closed without
an available main. Remove obsolete `agents.default_role`, `inter_session.receiver`,
and role `main` keys; they fail validation.

Startup, serve bootstrap, and previews default to the effective main role, or
ordered role fallback without main. `TAU_STARTUP_ROLE` overrides it; CLI `--role`
wins over the environment. Role selection does not bind main identity, change
saved roles, rerun bootstrap IDs, or change external bridge receiver policies.

`tool_policy.rules` is harness-owned declarative tool-surface policy. Rules are
keyed so a user can disable built-ins such as `builtin.chatgpt-shell` with
`enable: false`; matching rules run `disable_tool_tags` before
`enable_tool_tags`. Rules sort by `priority` (default `0`, lower first) and then
rule name. Tag patterns are exact or terminal-prefix forms like `shell:*`. Rule
names may contain dots, so CLI overrides should use a whole-map value, for
example `tool_policy={{{{rules: {{{{builtin.chatgpt-shell: {{{{enable: false}}}}}}}}}}}}`.

Agent-global, group, and role `required_skills` add/deduplicate exact names.
Missing, hidden, or unreadable skills emit `harness.config_error` and disable
the role; a disabled startup/default role fails startup without fallback.

Group/role `required_tools` add/deduplicate exact model-visible names, for example
`required_tools: [zulip_register, zulip_send]`, not extension names or hosted
provider capabilities. Tau checks availability after extension startup, including
tool policy/model support. Missing tools fail the selected role **and restored
agents' saved role/model on resume**, before recovery, initialization, or replay
dispatch. New-agent requests recheck availability.

Fix extension availability, role policy, model support, or the requirement.
This guard neither monitors/kills running agents after tool loss nor rolls back
extension startup side effects.

### CLI UI defaults

```yaml
{ui_config}
```

## Extension availability

Configure `extensions.<name>` in `harness.yaml`. `enable: false` disables it.
Enabled extensions default to `require: true`: startup/config/secret/spawn
failures are fatal. `require: false` skips the extension on startup or pre-ready
failure and emits a warning `harness.notice` alert.
Notices have stable `kind` strings and levels `critical`, `warning`, `info`,
`debug`, `trace`. Set `notice_level: warning` in `cli.yaml` or use
`:set notice-level warning`; this filters diagnostics, never the alert.

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
