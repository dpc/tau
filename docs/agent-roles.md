# Agent roles

Agent roles are named aliases for the model and model-behavior settings Tau should use for agent turns.

A role can set:

- `description`: short free-form summary shown in `/role ...` completions
- `order`: optional numeric order within the containing role group
- `model`: qualified model id, in `provider/model` form
- `effort`: `off`, `minimal`, `low`, `medium`, `high`, or `xhigh`
- `verbosity`: `low`, `medium`, or `high`
- `thinking_summary`: `off`, `auto`, `concise`, or `detailed`
- `service_tier`: `fast` or `flex`
- `compaction`: provider-side automatic compaction policy: `provider_default`, `disabled`, or `{ threshold: 200000 }`
- `prompt_fragments`: role-specific prompt fragments
- `prompt_override`: system prompt template name
- `tools`: explicit internal tools enabled for this role
- `disable_tool_tags`: tool tag patterns removed after global policy
- `enable_tool_tags`: tool tag patterns added after role tag disables
- `disable_tool_groups`: tool groups removed after role tag changes
- `enable_tool_groups`: tool groups added after role group disables
- `disable_tools`: internal tools removed after role group changes
- `enable_tools`: internal tools added last

System prompt templates receive `agent_id` when Tau dispatches a prompt for a
concrete agent. Tau's built-in templates render it at the end in an "Agent
identity" section; custom templates selected with `prompt_override` can place or
word `{{agent_id}}` however they want. `tau dev print-prompt` and
`tau dev print-system-prompt` use the stable fake `dev-preview-agent` id so
role previews show the full template.

Top-level `prompt_fragments` in `harness.yaml` apply to every role. Use them for global style or policy instructions:

```yaml
prompt_fragments:
  - name: user.short-plain-style
    priority: 65
    text: Keep answers short and plain, using only simple words.
```

Roles live in `harness.yaml` under globally unique `role_groups`. Each group has a `roles` map, plus optional role fields such as `prompt_fragments` that apply as defaults to every role in the group. `default_role` selects the startup role; if omitted, Tau starts on the first role in `role_groups` order. Within a group, role cycling sorts by `order` first and role name second; roles without `order` sort after ordered roles by name.

```json5
{
  default_role: "senior-engineer",
  role_groups: {
    engineer: {
      prompt_fragments: [
        { name: "engineer.workflow", priority: 66, text: "Focus on implementation details." },
      ],
      roles: {
        "junior-engineer": {
          order: 10,
          description: "Lower-reasoning engineer",
          effort: "low",
        },
        "senior-engineer": {
          order: 20,
          description: "Balanced coding engineer",
          model: "chatgpt/gpt-5.3-codex",
          effort: "medium",
          compaction: { threshold: 200000 },
          tools: ["read", "grep"],
          enable_tool_groups: ["calendar", "email"],
          disable_tools: ["email_trash"],
        },
        "staff-engineer": {
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
}
```

Missing fields use group defaults first, then provider-published fallback knobs for the role's resolved model. Tools start from extension default enablement, then harness `tool_policy.rules` apply by provider/tool tags. Role overrides run afterward in broad-to-specific order: `disable_tool_tags`, `enable_tool_tags`, `disable_tool_groups`, `enable_tool_groups`, `disable_tools`, then `enable_tools`. `tools` remains an explicit role allow-list base when set. This order lets a role disable `shell:*` and keep `shell:cd`, or disable a group and keep one named tool. When `compaction` is omitted, Tau asks supported providers to use their model-specific compaction default. Set `enable: false` on a role in a higher-precedence config layer to remove it from the effective role list and role-group cycling after all layers merge.

Global harness policy is configured under `tool_policy.rules` keyed by stable rule name. Rules default to `enable: true`, can be disabled with `enable: false`, match when all `when.model_tags` patterns match the selected model, then run `disable_tool_tags` before `enable_tool_tags`. Rules sort by `priority` (default `0`, lower runs first) and then by rule name for ties. Tag patterns are exact (`shell:cd`) or terminal prefix wildcards (`shell:*`, `shell:edit:*`). Built-in rule `builtin.chatgpt-shell` matches `shell:chatgpt`, disables `shell:*`, and re-enables `shell:edit:apply_patch`, `shell:exec:shell_command`, `shell:cd`, and `shell:lock`. Rule names may contain dots; for CLI overrides, prefer the whole-map form such as `--harness-config 'tool_policy={rules: {builtin.chatgpt-shell: {enable: false}}}'` rather than dotted paths through the rule name.

Tau ships built-in `junior-engineer`, `senior-engineer`, `staff-engineer`, and `micro-manager` roles, with `default_role: senior-engineer`. `junior-engineer` uses lower reasoning for straightforward engineering work, `senior-engineer` uses balanced individual-contributor defaults, and `staff-engineer` is the maximum-reasoning engineering variant. `micro-manager` is an orchestration role with a built-in delegation prompt. For non-trivial work, the built-in `micro-manager` prompt tells the model to use `agent_start` by default for research/scoping, implementation, and review/validation sub-agent steps, then synthesize the results; tiny or purely clerical work may still be handled directly.


## Selecting a role

Use `/role <role>`.

`/role` completion lists roles. Each completion description shows the currently resolved model and role settings, and appends the configured role `description` when present. `/model <provider>/<model>` switches the model for the currently selected agent without changing the role. After `/new`, when no current agent is selected yet, `/model <provider>/<model>` stages a one-shot override for the next agent created by the first prompt.


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

The `<role>` argument completes existing roles, but any new name can be used to create a role for the current run. Add it to `role_groups` if it should be available after restart.

`/role <role> delete` removes the runtime role override. It does not edit `role_groups` from configuration; built-in or configured roles come back on the next harness start.

Runtime role changes are not persisted. Startup is controlled by `default_role` and `role_groups` order, and durable role changes should be made in `harness.yaml`.

Prompt fragment priorities sort ascending. Use priorities below `100` for role/persona instructions that should appear before generated context sections such as skills and AGENTS.md. Use high priorities for epilogue context; Tau's built-in current-working-directory fragment uses `900` so it stays at the end of the prompt.
