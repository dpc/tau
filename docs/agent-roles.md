# Agent roles

Agent roles are named aliases for the model and model-behavior settings Tau should use for agent turns.

Harness configuration can place role-default patches under
`profiles.default`. Tau selects that built-in profile whenever neither
`--profile` nor `TAU_PROFILE` names one, so it is a useful place for ordinary
local defaults without repeating base configuration. `--profile focused`
selects only `profiles.focused`; it does not inherit `profiles.default`.

`agents`, a role group, and a role can set these provider/model fields:

- `model`: qualified model id, in `provider/model` form
- `effort`: `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, or `max`
- `verbosity`: `low`, `medium`, or `high`
- `thinking_summary`: `off`, `auto`, `concise`, or `detailed`
- `service_tier`: `fast` or `flex`
- `compaction`: automatic compaction policy: `provider_default`, `disabled`, or `{ threshold: 200000 }`; the harness schedules standalone compaction for models that publish it, while legacy models may use inline provider context management

For `effort`, `verbosity`, and `thinking_summary`, use `increase` or
`decrease` to adjust an inherited value by one level, or `increase:<amount>` /
`decrease:<amount>` for a saturating multi-level adjustment. Relative settings
resolve from the built-in bases (`medium`, `medium`, and `auto`) when no broader
absolute setting exists.

A role can also set:

- `description`: short free-form summary shown in `:role ...` completions
- `order`: optional numeric order within the containing role group
- `context_size_alerts`: named token thresholds that queue configurable internal
  prompts after a turn; committed deliveries appear in UI history as
  `[tau-internal]: <configured message>`
- `prompt_fragments`: role-specific prompt fragments
- `prompt_override`: system prompt template name
- `tools`: explicit internal tools enabled for this role
- `disable_tool_tags`: tool tag patterns removed after global policy
- `enable_tool_tags`: tool tag patterns added after role tag disables
- `disable_tool_groups`: tool groups removed after role tag changes
- `enable_tool_groups`: tool groups added after role group disables
- `disable_tools`: internal tools removed after role group changes
- `enable_tools`: internal tools added last
- `required_skills` / `requiredSkills`: exact skill names that must be
  discoverable and model-loadable before the role is available

System prompt templates receive `agent_id` when Tau dispatches a prompt for a
concrete agent. Tau's built-in templates render it at the end in an "Agent
identity" section; custom templates selected with `prompt_override` can place or
word `{{agent_id}}` however they want. `tau dev print-prompt` and
`tau dev print-system-prompt` use the stable fake `dev-preview-agent` id so
role previews show the full template.

Templates also receive sparse, deterministic runtime capabilities:

```handlebars
{{#if (tool_available capabilities.tools "web_search")}}Use web search.{{/if}}
{{#if (extension_enabled capabilities.extensions "std-pim")}}PIM is enabled.{{/if}}
{{#if (extension_active capabilities.extensions "std-pim")}}PIM is ready.{{/if}}
```

`capabilities.tools.available` contains only sorted model-visible tool names
authorized for the concrete agent, role, model, provider tool types, and turn.
`capabilities.extensions.enabled` includes final startup-enabled names even when
an optional extension failed to start; `active` contains only Ready runtimes.
A valid absent name returns false. Invalid names, argument types/arity, missing
structured paths, unknown selected templates, and render failures are errors.
Each new turn uses current state; role previews use the role-resolved model.

`agents.prompt_fragments` in `harness.yaml` apply to every role in every
role group. Use them for global style, policy, or run-wide instructions that
should not be duplicated under each group or role:

```yaml
agents:
  prompt_fragments:
    - name: user.short-plain-style
      priority: 65
      text: Keep answers short and plain, using only simple words.
```

Use the same top-level `agents` scope for model defaults shared by every role.
`agents.enable` defaults to `true`; set it to `false` to disable every role,
then use a group or role `enable: true` override to retain the roles you need.
Tau retains source order within each scope, but resolves scopes globally: all
`agents` settings apply first, then all role-group settings, then all role
settings. A role setting therefore overrides a group setting even when the
group setting comes from a later drop-in, selected profile, or
`--harness-config` layer:

```yaml
agents:
  model: chatgpt/gpt-5.3-codex
  effort: medium
  role_groups:
    review:
      effort: high
      roles:
        reviewer:
          effort: xhigh
```

The same global fragment path is available to one-shot harness config overrides,
for example `--harness-config 'agents.promptFragments=[{ name: "run.policy", priority:
65, text: "Follow the run policy." }]'`.

## Configuration profiles

Use `profiles` for named, opt-in role adjustments without replacing normal base
configuration. `tau --profile focused` selects a profile; `TAU_PROFILE=focused`
does the same when the flag is absent. Tau rejects an unknown name. The selected
profile loads after `harness.yaml` and `harness.d` files, but before
`--harness-config` and role CLI overrides, so a relative profile setting uses the
base setting as its starting point:

```yaml
agents:
  effort: low

profiles:
  focused:
    agents:
      effort: increase
      role_groups:
        review:
          roles:
            reviewer:
              verbosity: high
```

Profiles support `agents.default_role`, agent defaults including `agents.enable`,
global role metadata, role groups, and role patches. A profile can select a role
it creates or enables. Its selected default supersedes the base setting, while a
later `--harness-config agents.default_role=...` override supersedes the
profile. Set `agents.default_role: null` in a profile to clear a base default and
fall back to configured role order. The `defaultRole` alias is also accepted.
Profiles do not expose unrelated harness settings; keep those in base files or
use a normal command-line override.

`tau component harness` runs in the current process, so it cannot apply
`--profile`; set `TAU_PROFILE` before launching that component instead. Normal
`tau --profile NAME` startup and render commands forward the resolved selection
to their spawned harness daemon.

Roles live in `harness.yaml` under globally unique `agents.role_groups`. Each group has a `roles` map, plus optional role fields such as `prompt_fragments` that apply as defaults to every role in the group. `agents.default_role` selects the startup role; if omitted, Tau starts on the first role in `agents.role_groups` order. Within a group, role cycling sorts by `order` first and role name second; roles without `order` sort after ordered roles by name.

Named context-size alerts can be set globally, on a role group, or on one role.
Their fields merge from broad to specific scope, so a role can customize or
disable an inherited alert. `enable` defaults to `true`, and `message` defaults
to `Use the \`compact\` tool after finishing your current task.`. Every new alert
requires a positive `threshold`; an explicitly configured `message` cannot be
empty:

```yaml
agents:
  context_size_alerts:
    compact-soon:
      threshold: 160000
  role_groups:
    engineer:
      roles:
        reviewer:
          context_size_alerts:
            compact-soon:
              enable: false
```

When completed inference reports input usage strictly above the threshold, Tau
queues the message as an internal prompt after the current response and any tool
calls. When it reaches the agent, the UI history shows
`[tau-internal]: <configured message>` at that delivery point, including after
late attach or resume. Failed and compaction responses do not fire alerts.
During one running Tau daemon, each alert fires once until usage falls back to
or below its threshold or context accounting resets. Alert crossing and
queued-delivery state is advisory runtime state and is not reconstructed after
a restart; only a delivery that already committed remains in history. Alerts
only advise the model; they do not grant `compact` if role policy disables the
tool.

Roles opt into bare inter-session messages with ordinary inherited capabilities.
`inter_session_receiver` allows a role's agents to receive them, while
`inter_session_auto_start` also allows Tau to start that role when no live
receiver exists:

```yaml
agents:
  role_groups:
    coordination:
      inter_session_receiver: true
      inter_session_auto_start: true
      roles:
        project-manager:
          order: 0
        task-manager:
          order: 10
          inter_session_auto_start: false
    engineer:
      roles:
        engineer:
          inter_session_receiver: true
          inter_session_auto_start: true
```

Group values are defaults and role values override them with normal role
layering; `null` clears an inherited value. Camel-case
`interSessionReceiver`/`interSessionAutoStart` aliases are also accepted.
Auto-start requires receiver capability. Multiple groups and multiple
auto-start roles are valid.

Live routing keeps idle/least-recently-routed fairness across all eligible
receiver roles. If none is live, Tau walks roles in configured group order and
then within-group `order`/name order, skipping disabled or currently unavailable
roles and models. The new endpoint uses that role's normal model, skill, prompt,
and tool policy and does not inherit the remote sender's parent, cwd, transcript,
or watches. Busy eligible receivers are reused instead of creating more agents.
The removed `peer_entrypoint`/`auto_start_role` schema is not accepted.

```json5
{
  agents: {
    default_role: "engineer",
    role_groups: {
      engineer: {
        prompt_fragments: [
          { name: "engineer.workflow", priority: 66, text: "Focus on implementation details." },
        ],
        roles: {
          "engineer-junior": {
            order: 10,
            description: "Lower-reasoning engineer",
            effort: "low",
          },
          "engineer": {
            order: 20,
            description: "Balanced coding engineer",
            model: "chatgpt/gpt-5.3-codex",
            effort: "medium",
            compaction: { threshold: 200000 },
            tools: ["read", "grep"],
            enable_tool_groups: ["calendar", "email"],
            disable_tools: ["email_trash"],
            requiredSkills: ["project-review-process"],
          },
          "engineer-senior": {
            order: 30,
            description: "Maximum-reasoning engineer",
            effort: "xhigh",
          },
          "old-role": {
            enable: false,
          },
        },
      },
      review: {
        roles: {
          "reviewer": {
            order: 10,
            prompt_fragments: [
              { name: "review.workflow", priority: 66, text: "Review changes carefully." },
            ],
          },
        },
      },
    },
  },
}
```

Missing provider/model fields use `agents` defaults first, then group defaults, then provider-published fallback knobs for the role's resolved model. `required_skills` from `agents`, groups, and roles are additive and de-duplicated. After startup skill discovery, Tau disables any role whose required skills cannot be found by exact name, are hidden from model-side skill loading, or cannot be read; this emits a mandatory `harness.config_error` notice and removes the role from selection and delegation. If the selected/default startup role is disabled this way, startup fails clearly instead of falling back to another role. Tools start from extension default enablement, then harness `tool_policy.rules` apply by provider/tool tags. Role overrides run afterward in broad-to-specific order: `disable_tool_tags`, `enable_tool_tags`, `disable_tool_groups`, `enable_tool_groups`, `disable_tools`, then `enable_tools`. `tools` remains an explicit role allow-list base when set. This order lets a role disable `shell:*` and keep `shell:workdir`, or disable a group and keep one named tool. When `compaction` is omitted, Tau uses the model's published standalone threshold when available, otherwise it asks an inline-capable provider to use its model-specific default. Set `enable: false` on a role in a higher-precedence config layer to remove it from the effective role list and role-group cycling after all layers merge.

Global harness policy is configured under `tool_policy`. Set `default_shell_tool_style` to `codex`, `edit`, or `replace` to choose the global file-edit surface; missing, null, or whitespace-only values use the model default. Rules under `tool_policy.rules` are keyed by stable rule name. Rules default to `enable: true`, can be disabled with `enable: false`, match when all `when.model_tags` patterns match the selected model, then run `disable_tool_tags` before `enable_tool_tags`. Rules sort by `priority` (default `0`, lower runs first) and then by rule name for ties. Tag patterns are exact (`shell:workdir`) or terminal prefix wildcards (`shell:*`, `shell:edit:*`). Built-in rule `builtin.chatgpt-shell` matches `shell:chatgpt`, disables `shell:*`, and re-enables `shell:edit:apply_patch`, `shell:read:image`, `shell:exec:shell_command`, `shell:workdir`, and `shell:lock`; image-producing tools remain independently gated by the exact provider route modalities. Rule names may contain dots; for CLI overrides, prefer the whole-map form such as `--harness-config 'tool_policy={rules: {builtin.chatgpt-shell: {enable: false}}}'` rather than dotted paths through the rule name.

Tau ships built-in `engineer-junior`, `engineer`, and `engineer-senior` roles,
with `agents.default_role: engineer`. `engineer-junior` uses lower reasoning
for straightforward engineering work, `engineer` uses balanced
individual-contributor defaults, and `engineer-senior` is the
maximum-reasoning engineering variant.

For every role whose effective tool surface includes `agent_start`, Tau's
built-in global prompt fragment lists the currently available sub-task roles.
Tau omits the fragment from prompt template data when `agent_start` is
unavailable, including when role or model policy removes the tool.


## Selecting a role

Use `:role <role>`.

`:role` completion lists roles. Each completion description shows the currently
resolved model and role settings, and appends the configured role `description`
when present. `:new <role>` also completes roles; it clears the current agent
selection and makes the next prompt create a new agent with that role. Later
no-agent role selections such as `:role <role>` or role cycling supersede the
role named in `:new <role>`.

`:model <provider>/<model>` switches the model for the currently selected agent
without changing the role. After `:new`, when no current agent is selected yet,
`:model <provider>/<model>` stages a one-shot override for the next agent
created by the first prompt.


## Editing roles

Use:

```text
:role <role> <delete|model|effort|verbosity|thinking-summary|service-tier|compaction-threshold|tools|enable-tool-groups|disable-tool-groups|enable-tools|disable-tools> [value]
```

Examples:

```text
:role engineer model chatgpt/gpt-5.3-codex
:role engineer-senior effort xhigh
:role engineer enable-tool-groups calendar,email
:role engineer disable-tools email_trash
:role temporary model anthropic/claude-sonnet-4-20250514
:role temporary delete
```

Use `reset` as the value to clear a field and return to model/provider fallback behavior (`off` is still the explicit off value for `effort` and `thinking-summary`).

For `effort`, `verbosity`, and `thinking-summary`, `increase`,
`decrease`, `increase:<amount>`, and `decrease:<amount>` adjust the current
effective role value and saturate at each setting's endpoints. This differs
from configuration, where relative values resolve broadly from `agents` through
the role group to the role before Tau stores the absolute result.

The convenience command `:fast` mutates the currently selected role using the same role-update path.

The `<role>` argument completes existing roles, but any new name can be used to create a role for the current run. Add it to `agents.role_groups` if it should be available after restart.

`:role <role> delete` removes the runtime role override. It does not edit `agents.role_groups` from configuration; built-in or configured roles come back on the next harness start.

Runtime role changes are not persisted. Startup is controlled by `agents.default_role` and `agents.role_groups` order, and durable role changes should be made in `harness.yaml`.

Prompt fragment priorities sort ascending. Use priorities below `100` for role/persona instructions that should appear before generated context sections such as skills and AGENTS.md. Use high priorities for epilogue context; Tau's built-in current-working-directory fragment uses `900` so it stays at the end of the prompt.
## Compaction tool policy

Self-compaction is enabled by default. Disable it for a role by group or exact
tool name:

```yaml
disable_tool_groups: [compaction]
# or: disable_tools: [compact]
```

Cross-agent compaction remains disabled by default and requires an explicit
`enable_tool_groups: [cross_agent_compaction]` or
`enable_tools: [agent_compact]`. `RoleCompaction::Disabled` controls automatic
compaction and does not disable the model-callable `compact` tool. The
cross-agent tool authorizes any other loaded same-session agent; ancestry,
watching, and messaging are irrelevant to that explicit capability.

## Discovery tool opt-in

`session_list` and `agent_list` are independently disabled by default:

```yaml
enable_tool_groups:
  - session_discovery
  - agent_discovery
```

`session_list` returns only bounded, basename-redacted live sessions available
for inter-session messaging. `agent_list` returns a bounded
current-session-only snapshot of agent id, creation role/group,
`pending|idle|running`, and self status. Neither tool grants messaging, watching,
starting, or compaction authority.
