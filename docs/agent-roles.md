# Agent roles

Agent roles are named aliases for the model and model-behavior settings Tau should use for agent turns.

A role can set:

- `description`: short free-form summary shown in `/role ...` completions
- `order`: optional numeric order within the containing role group
- `model`: qualified model id, in `provider/model` form
- `effort`: `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, or `max`
- `verbosity`: `low`, `medium`, or `high`
- `thinking_summary`: `off`, `auto`, `concise`, or `detailed`
- `service_tier`: `fast` or `flex`
- `compaction`: automatic compaction policy: `provider_default`, `disabled`, or `{ threshold: 200000 }`; the harness schedules standalone compaction for models that publish it, while legacy models may use inline provider context management
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

The same global fragment path is available to one-shot harness config overrides,
for example `--harness-config 'agents.promptFragments=[{ name: "run.policy", priority:
65, text: "Follow the run policy." }]'`.

Roles live in `harness.yaml` under globally unique `agents.role_groups`. Each group has a `roles` map, plus optional role fields such as `prompt_fragments` that apply as defaults to every role in the group. `agents.default_role` selects the startup role; if omitted, Tau starts on the first role in `agents.role_groups` order. Within a group, role cycling sorts by `order` first and role name second; roles without `order` sort after ordered roles by name.

At most one effective group may opt into same-UID peer rendezvous with
`peer_entrypoint: {}`. The independent `auto_start_role` field is the only grant
that permits a future bare peer message to start a model-backed endpoint, and it
must explicitly name an enabled role in that group:

```yaml
agents:
  role_groups:
    external:
      peer_entrypoint:
        auto_start_role: external
      roles:
        external: {}
```

Higher-precedence `peer_entrypoint: null` removes routing/discovery authority;
`auto_start_role: null` removes only auto-start authority. Group ordering never
selects a spending role.

When no eligible endpoint exists, peer routing may start only `auto_start_role`.
The new endpoint uses that role's normal model, skill, prompt, and tool policy and
does not inherit the remote sender's parent, cwd, transcript, or watches. Busy
eligible endpoints are reused instead of creating more agents.

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
      manager: {
        roles: {
          "micro-manager": {
            order: 10,
            prompt_fragments: [
              { name: "manager.workflow", priority: 66, text: "Delegate non-trivial work." },
            ],
          },
        },
      },
    },
  },
}
```

Missing fields use group defaults first, then provider-published fallback knobs for the role's resolved model. `required_skills` from `agents`, groups, and roles are additive and de-duplicated. After startup skill discovery, Tau disables any role whose required skills cannot be found by exact name, are hidden from model-side skill loading, or cannot be read; this emits a mandatory `harness.config_error` notice and removes the role from selection and delegation. If the selected/default startup role is disabled this way, startup fails clearly instead of falling back to another role. Tools start from extension default enablement, then harness `tool_policy.rules` apply by provider/tool tags. Role overrides run afterward in broad-to-specific order: `disable_tool_tags`, `enable_tool_tags`, `disable_tool_groups`, `enable_tool_groups`, `disable_tools`, then `enable_tools`. `tools` remains an explicit role allow-list base when set. This order lets a role disable `shell:*` and keep `shell:cd`, or disable a group and keep one named tool. When `compaction` is omitted, Tau uses the model's published standalone threshold when available, otherwise it asks an inline-capable provider to use its model-specific default. Set `enable: false` on a role in a higher-precedence config layer to remove it from the effective role list and role-group cycling after all layers merge.

Global harness policy is configured under `tool_policy.rules` keyed by stable rule name. Rules default to `enable: true`, can be disabled with `enable: false`, match when all `when.model_tags` patterns match the selected model, then run `disable_tool_tags` before `enable_tool_tags`. Rules sort by `priority` (default `0`, lower runs first) and then by rule name for ties. Tag patterns are exact (`shell:cd`) or terminal prefix wildcards (`shell:*`, `shell:edit:*`). Built-in rule `builtin.chatgpt-shell` matches `shell:chatgpt`, disables `shell:*`, and re-enables `shell:edit:apply_patch`, `shell:exec:shell_command`, `shell:cd`, and `shell:lock`. Rule names may contain dots; for CLI overrides, prefer the whole-map form such as `--harness-config 'tool_policy={rules: {builtin.chatgpt-shell: {enable: false}}}'` rather than dotted paths through the rule name.

Tau ships built-in `engineer-junior`, `engineer`, `engineer-senior`, and `micro-manager` roles, with `agents.default_role: engineer`. `engineer-junior` uses lower reasoning for straightforward engineering work, `engineer` uses balanced individual-contributor defaults, and `engineer-senior` is the maximum-reasoning engineering variant. `micro-manager` is an orchestration role with a built-in delegation prompt. For non-trivial work, the built-in `micro-manager` prompt tells the model to use `agent_start` by default for research/scoping, implementation, and review/validation sub-agent steps, then synthesize the results; tiny or purely clerical work may still be handled directly.


## Selecting a role

Use `/role <role>`.

`/role` completion lists roles. Each completion description shows the currently
resolved model and role settings, and appends the configured role `description`
when present. `/new <role>` also completes roles; it clears the current agent
selection and makes the next prompt create a new agent with that role. Later
no-agent role selections such as `/role <role>` or role cycling supersede the
role named in `/new <role>`.

`/model <provider>/<model>` switches the model for the currently selected agent
without changing the role. After `/new`, when no current agent is selected yet,
`/model <provider>/<model>` stages a one-shot override for the next agent
created by the first prompt.


## Editing roles

Use:

```text
/role <role> <delete|model|effort|verbosity|thinking-summary|service-tier|compaction-threshold|tools|enable-tool-groups|disable-tool-groups|enable-tools|disable-tools> [value]
```

Examples:

```text
/role engineer model chatgpt/gpt-5.3-codex
/role micro-manager effort xhigh
/role engineer enable-tool-groups calendar,email
/role engineer disable-tools email_trash
/role temporary model anthropic/claude-sonnet-4-20250514
/role temporary delete
```

Use `reset` as the value to clear a field and return to model/provider fallback behavior (`off` is still the explicit off value for `effort` and `thinking-summary`).

The convenience command `/fast` mutates the currently selected role using the same role-update path.

The `<role>` argument completes existing roles, but any new name can be used to create a role for the current run. Add it to `agents.role_groups` if it should be available after restart.

`/role <role> delete` removes the runtime role override. It does not edit `agents.role_groups` from configuration; built-in or configured roles come back on the next harness start.

Runtime role changes are not persisted. Startup is controlled by `agents.default_role` and `agents.role_groups` order, and durable role changes should be made in `harness.yaml`.

Prompt fragment priorities sort ascending. Use priorities below `100` for role/persona instructions that should appear before generated context sections such as skills and AGENTS.md. Use high priorities for epilogue context; Tau's built-in current-working-directory fragment uses `900` so it stays at the end of the prompt.
## Compaction tool opt-in

Both tools are disabled by default and configured independently:

```yaml
enable_tool_groups: [compaction]              # only compact {}
enable_tool_groups: [cross_agent_compaction]  # only agent_compact {agent_id}
```

Exact `enable_tools` entries may be used instead. `RoleCompaction::Disabled`
controls automatic compaction and does not override an explicit tool opt-in.
The cross-agent tool authorizes any other loaded same-session agent; ancestry,
watching, and messaging are irrelevant to that explicit capability.

## Discovery tool opt-in

`session_list` and `agent_list` are independently disabled by default:

```yaml
enable_tool_groups:
  - session_discovery
  - agent_discovery
```

`session_list` returns only bounded, basename-redacted live sessions that confirm
an entrypoint. `agent_list` returns a bounded current-session-only snapshot of
agent id, creation role/group, `pending|idle|running`, and self status. Neither
tool grants messaging, watching, starting, or compaction authority.
