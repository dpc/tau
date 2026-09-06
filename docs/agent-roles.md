# Agent roles

Agent roles are named aliases for the model and model-behavior settings Tau should use for agent turns.

Harness configuration can name a fallback profile with top-level
`default_profile: local`. Tau uses it when neither `--profile` nor
`TAU_PROFILE` names profiles; omit it, or set it to `null`, to use only base
configuration. `--profile focused,review` applies only `profiles.focused` then
`profiles.review`; it does not inherit the fallback profile.

`agents`, a role group, and a role can set these provider/model fields:

- `model`: qualified model id, in `provider/model` form
- `effort`: `provider_default`, `disabled`, or an exact decimal intensity from
  `0.0` through `1.0`
- `verbosity`: `low`, `medium`, or `high`
- `thinking_summary`: `off`, `auto`, `concise`, or `detailed`
- `service_tier`: `fast` or `flex`
- `inference_compaction`: singular provider-inline and reactive-overflow policy:
  `provider_default`, `disabled`, `{ threshold: 200000 }`, or
  `{ reserve: 25000 }`
- `compactions`: named harness-scheduled standalone policies; each selects a
  `threshold` or `reserve` boundary plus an optional lifecycle/status condition
- `compaction`: legacy shorthand normalized into both domains above

For `effort`, use `increase:<decimal>` or `decrease:<decimal>` with a positive
magnitude. Relative effort keeps the exact signed millionth result even outside
`0.0..=1.0`; the selected model clamps only when mapping a prompt to its native
level. An otherwise-unset effort adjustment starts from `0.5`.
For `verbosity` and `thinking_summary`, `increase`/`decrease` and optional
integer amounts retain their saturating level behavior.

A role can also set:

- `description`: short free-form summary shown in `:role ...` completions
- `order`: optional numeric order within the containing role group
- `visible`: whether the role appears in the built-in available-sub-task-role
  prompt catalog; defaults to `true`
- `context_size_alerts`: named token thresholds that queue configurable internal
  prompts after a turn; committed deliveries appear in UI history as
  `□ <configured message>` in the dedicated internal-notice style
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
concrete agent. Tau's built-in templates intentionally omit it because agents
can query authoritative runtime identity with `self_info`; custom templates
selected with `prompt_override` can still place or word `{{agent_id}}` however
they want. `tau dev print-prompt` and `tau dev print-system-prompt` use the
stable fake `dev-preview-agent` input when rendering custom templates.

Templates receive `session.cwd` as the canonical current directory captured when
the harness started. This is a session-wide startup value. It differs from `cwd`
and `working_directory`, which describe extension-published, per-agent shell
workdir state and can change through the `workdir` tool. For example, match the
session path directly without enumerating agent context:

```handlebars
{{#if (eq session.cwd "/home/dpc/lab")}}
Apply these instructions outside the excluded project.
{{/if}}
```

`tau dev print-prompt` and `tau dev print-tools` use the role that normal startup
would select when `--role` is omitted, including profile and configured-default
resolution. Pass `--role NAME` to preview that explicit role instead.
Both commands configure ordinary extensions, load one fresh ephemeral agent,
wait boundedly for its per-agent context, and resolve one model/tool snapshot.
They do not call a provider or create a resumable session. Extensions still use
their ordinary User, Cache, Secret, direct-state, filesystem, and network
semantics, so running either diagnostic can have the same extension side effects
as ordinary startup. This is fresh-agent parity, not a view of a restored or
currently running agent.

`tau dev print-tools` applies the same logical-web selection as a live prompt.
When the exact route selects provider-hosted search, the JSON entry uses
`"name": "web_search"` and `"execution": "provider_native"` rather than
claiming that an ordinary Tau function is present. If exact model capability
metadata is unavailable, the command prints its provisional ordinary surface
and emits a warning that provider-native replacement could not be resolved.

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
`agents.visible` also defaults to `true` and inherits through groups and roles.
Set it to `false` to omit roles from the built-in catalog shown to agents with
`agent_start`; a narrower `visible: true` overrides that default. Visibility
only affects this prompt catalog. Hidden roles remain selectable, explicitly
callable through `agent_start`, and present in authorization, diagnostics, UI,
and other role lists.
Tau retains source order within each scope, but resolves scopes globally: all
`agents` settings apply first, then all role-group settings, then all role
settings. A role setting therefore overrides a group setting even when the
group setting comes from a later drop-in, selected profile, or
`--harness-config` layer:

```yaml
agents:
  model: chatgpt/gpt-5.3-codex
  effort: 0.5
  role_groups:
    review:
      effort: 0.75
      roles:
        reviewer:
          effort: 0.9
```

The same global fragment path is available to one-shot harness config overrides,
for example `--harness-config 'agents.promptFragments=[{ name: "run.policy", priority:
65, text: "Follow the run policy." }]'`.

## Provider and model aliases

Use startup-only aliases when roles should keep stable names while a profile,
environment variable, or one launch selects a different provider account or
exact model:

```yaml
aliases:
  providers:
    subscription: codex-work
  models:
    current: gpt-5.5
agents:
  model: subscription/current
```

Tau resolves this to `codex-work/gpt-5.5` after all role and profile merging.
Aliases may chain; an identity mapping such as `subscription: subscription`
stops resolution and restores the literal name. Other cycles fail startup.
Model aliases match the entire suffix after the first `/`, so
`openrouter/sonnet` can map `sonnet` to `anthropic/claude-sonnet-4`.

Profiles may override either map. For one launch, JSON-object environment
variables apply after config (`TAU_PROVIDER_ALIASES='{"subscription":"codex-work"}'`
and `TAU_MODEL_ALIASES='{"current":"gpt-5.5"}'`), then repeatable
`--provider-alias subscription=codex-personal` and
`--model-alias current=gpt-5.6` flags apply last.

For example, a profile can switch the stable account name without duplicating
role definitions:

```yaml
profiles:
  personal:
    aliases:
      providers:
        subscription: codex-personal
  work:
    aliases:
      providers:
        subscription: codex-work
```

`tau --profile work` then resolves every static `subscription/...` role model
through the `codex-work` provider profile.

Aliases affect only static role configuration at harness startup. Interactive
`:model`, runtime `:role ... model`, protocol model overrides, persisted events,
and an already-running attached daemon use canonical model IDs and do not
resolve aliases.

## Configuration profiles

Use `profiles` for named, opt-in role adjustments without replacing normal base
configuration. `tau --profile focused,review` selects profiles in that exact
order; `TAU_PROFILE=focused,review` does the same when the flag is absent.
ASCII spaces and tabs around names are ignored, but empty or whitespace-only
segments and unknown names fail startup. Repeated names are applied repeatedly.
Otherwise,
`default_profile: focused` selects one fallback; omit it or set it to `null`
to source no profile. Its surrounding ASCII spaces/tabs are ignored, but it
cannot use comma-separated syntax. Selected profiles load after
`harness.yaml` and `harness.d` files, but before
`--harness-config` and role CLI overrides, so a relative profile setting uses the
accumulated base and earlier profiles as its starting point:

```yaml
agents:
  effort: 0.25

profiles:
  focused:
    agents:
      effort: "increase:0.25"
      role_groups:
        review:
          roles:
            reviewer:
              verbosity: high
```

Profiles support the global `tau_state_access` default, `agents.default_role`,
agent defaults including `agents.enable`, global role metadata, role groups,
role patches, and extension `enable` and arbitrary `config` patches. A profile
can select a role it creates or enables.
Its selected default supersedes the base setting, while a later
`--harness-config agents.default_role=...` override supersedes the profile. Set
`agents.default_role: null` in a profile to clear a base default and fall back
to configured role order. The `defaultRole` alias is also accepted. Extension
config maps merge recursively; arrays, scalars, nested nulls, and type
mismatches replace, while top-level `config: null` remains absent/no-op.
Profiles do not expose per-instance outer extension fields or unrelated harness
settings; keep those in base files or use a normal command-line override.

`tau component harness` runs in the current process, so it cannot apply
`--profile`; its normal base-configured `default_profile` still applies, and
set `TAU_PROFILE` before launching it to override that fallback. Normal
`tau --profile NAME[,NAME...]` startup and render commands forward the resolved selection
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
      when:
        at: after_response
        statuses: [working, waiting]
  role_groups:
    engineer:
      roles:
        reviewer:
          context_size_alerts:
            compact-soon:
              enable: false
```

Named automatic-compaction policies use the same broad-to-specific named merge:

```yaml
agents:
  role_groups:
    engineer:
      roles:
        main:
          inference_compaction: disabled
          compactions:
            eager:
              reserve: 40000
              when:
                at: outer_turn_finished
                statuses: [done]
            fallback:
              threshold: provider_default
              when:
                at: before_inference
```

Tau supplies a built-in `default` policy at `before_inference` with
`threshold: context_limit_safe`. Additional named policies augment it rather
than shadowing it. Set `compactions.default.enable: false` to opt out; legacy
`compaction: disabled` remains a replace-all opt-out. `context_limit_safe`
resolves the adapter-published safe scheduling threshold;
`provider_default` remains a compatibility spelling for the same value.
An explicit `reserve: N` resolves against the selected provider-qualified
model as `input_token_limit - N`; `threshold` and `reserve` are mutually
exclusive. The input limit is `min(context_window, max_input_tokens)` when the
provider publishes the optional maximum and otherwise falls back to
`context_window`. `reserve: 0` selects the full input limit. A reserve equal to
that limit resolves to zero and therefore supplies no proactive scheduling
authority; a larger reserve fails prompt validation with an actionable
role/policy/model diagnostic.

Matching policies at one lifecycle point OR together and produce one
standalone compaction using the lowest resolved matching threshold. Omitted
`statuses` means any status; an empty list is invalid. If the frozen prompt did
not expose the `status` tool, Tau treats an open turn as `working` and its
settled finish as `done` for policy matching only.

When completed inference reports input usage strictly above the threshold, Tau
queues the message as an internal prompt after the current response and any tool
calls. When it reaches the agent, the UI history shows
`□ <configured message>` in the dedicated internal-notice style at that delivery
point, including after late attach or resume. Failed and compaction responses do
not fire alerts.
Alerts may instead select `outer_turn_finished`; a matching successful final
queues a fresh internal-prompt turn after the durable finish. Automatic
compaction at that point remains idle and non-activating.
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
    effort: 0.5,
    role_groups: {
      engineer: {
        prompt_fragments: [
          { name: "engineer.workflow", priority: 66, text: "Focus on implementation details." },
        ],
        roles: {
          "engineer-junior": {
            order: 10,
            description: "Lower-reasoning engineer",
            effort: "decrease:0.15",
          },
          "engineer": {
            order: 20,
            description: "Balanced coding engineer",
            model: "chatgpt/gpt-5.3-codex",
            compaction: { reserve: 25000 },
            tools: ["read", "grep"],
            enable_tool_groups: ["calendar", "email"],
            disable_tools: ["email_trash"],
            requiredSkills: ["project-review-process"],
          },
          "engineer-senior": {
            order: 30,
            description: "Higher-reasoning engineer",
            effort: "increase:0.15",
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

Missing provider/model fields use `agents` defaults first, then group defaults, then provider-published fallback knobs for the role's resolved model. `required_skills` from `agents`, groups, and roles are additive and de-duplicated. After startup skill discovery, Tau disables any role whose required skills cannot be found by exact name, are hidden from model-side skill loading, or cannot be read; this emits a mandatory `harness.config_error` notice and removes the role from selection and delegation. If the selected/default startup role is disabled this way, startup fails clearly instead of falling back to another role. Tools start from extension default enablement, then harness `tool_policy.rules` apply by provider/tool tags. Role overrides run afterward in broad-to-specific order: `disable_tool_tags`, `enable_tool_tags`, `disable_tool_groups`, `enable_tool_groups`, `disable_tools`, then `enable_tools`. `tools` remains an explicit role allow-list base when set. This order lets a role disable `shell:*` and keep `shell:workdir`, or disable a group and keep one named tool. When the successor fields are omitted, `inference_compaction` uses the provider default and the built-in named `compactions.default` policy uses the model's published standalone threshold. Legacy `compaction` input sets both domains at its source layer, but later successor fields can override them independently. Set `enable: false` on a role in a higher-precedence config layer to remove it from the effective role list and role-group cycling after all layers merge.

Global harness policy is configured under `tool_policy`. Set `default_shell_tool_style` to `codex`, `edit`, or `replace` to choose the apply-patch, line-coordinate, or exact-text implementation. Missing, null, or whitespace-only values select exact-text `replace` except for ChatGPT/Codex models, which select `apply_patch`. Both non-Codex implementations are provider-visible as `edit`; their internal/configuration names remain distinct, so a role that enables both fails prompt construction with a duplicate visible name. Rules under `tool_policy.rules` are keyed by stable rule name. Rules default to `enable: true`, can be disabled with `enable: false`, match when all `when.model_tags` patterns match the selected model, then run `disable_tool_tags` before `enable_tool_tags`. Rules sort by `priority` (default `0`, lower runs first) and then by rule name for ties. Tag patterns are exact (`shell:workdir`) or terminal prefix wildcards (`shell:*`, `shell:edit:*`). Built-in rule `builtin.chatgpt-shell` matches `shell:chatgpt`, disables `shell:*`, and re-enables `shell:edit:apply_patch`, `shell:read:image`, `shell:exec:shell_command`, `shell:workdir`, and `shell:lock`; image-producing tools remain independently gated by the exact provider route modalities. Rule names may contain dots; for CLI overrides, prefer the whole-map form such as `--harness-config 'tool_policy={rules: {builtin.chatgpt-shell: {enable: false}}}'` rather than dotted paths through the rule name.

Tau ships built-in `engineer-junior`, `engineer`, and `engineer-senior` roles,
with `agents.default_role: engineer`. `engineer-junior` uses lower reasoning
for straightforward engineering work, `engineer` uses balanced
individual-contributor defaults, and `engineer-senior` is the
higher-reasoning engineering variant. Their built-in shared effort is 0.5,
so junior, engineer, and senior request 0.35, 0.5, and 0.65 respectively;
higher-precedence `agents.effort` settings rebase the relative presets.

For every role whose effective tool surface includes `agent_start`, Tau's
built-in global prompt fragment lists the available agent roles for
`agent_start`.
Tau omits the fragment from prompt template data when `agent_start` is
unavailable, including when role or model policy removes the tool.


## Selecting a role

Use `:role <role>`.

`:role` completion lists roles. Each completion description shows the currently
resolved model and non-tool role settings, and appends the configured role
`description` when present. It omits tool-policy fragments; complete tool
settings remain visible and editable through `:role <role> <setting>`
completion. `:new <role>` also completes roles; it clears the current agent
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
:role engineer-senior effort 0.9
:role engineer enable-tool-groups calendar,email
:role engineer disable-tools email_trash
:role temporary model anthropic/claude-sonnet-4-20250514
:role temporary delete
```

Use `reset` as the value to clear a field and return to its configured baseline.
Use `disabled` for explicit no-reasoning intent; `off` remains the explicit
disabled value for `thinking-summary`.

For `effort`, `increase:<decimal>` and `decrease:<decimal>` adjust the portable
requested intensity without clamping stored state. For `verbosity` and
`thinking-summary`, `increase`, `decrease`, and optional integer amounts adjust
and saturate the current value. Configuration resolves relative values broadly
from `agents` through the role group to the role before Tau stores the result.

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
`enable_tools: [agent_compact]`. `inference_compaction: disabled` disables only
provider-inline compaction and reactive-overflow recovery; named `compactions`
remain independent. Neither setting disables the model-callable `compact` tool. The
cross-agent tool authorizes any other loaded same-session agent; ancestry,
watching, and messaging are irrelevant to that explicit capability.

## Runtime self information

`self_info({})` is enabled by default and returns the calling agent's
authoritative runtime metadata as deterministic `key: value` lines. It accepts
no input fields and reports `agent_id`, `session_id`, `session_dir`, the exact
prompt-owned `model`, `effort_requested`, `effort_effective`, the latest
model-qualified provider-reported input and cached token counts, the model's
provider-advertised total context window and effective input-token capacity,
effective inference and named standalone compaction settings, optional validated
current-provider quota windows, `status`, and `status_task_name`. Context counts
are the last provider observation rather than a reconstruction of suffix growth
after that observation. Quota records
retain provider-normalized pool/window identity, observation age, reset and
relative remaining timing when available, and exact-model pool applicability;
they do not infer billing limits or make a pacing policy decision. Before the
first `status` call, it reports `status: unreported` and
`status_task_name: (none)`. An unavailable session directory is also `(none)`.
The built-in prompt templates omit agent identity; custom templates retain the
explicit `agent_id` input.

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
`pending|idle|running|restored_unavailable|stopped`, and self status. Neither tool grants messaging, watching,
starting, or compaction authority.
