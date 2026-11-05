---
name: tau-self-knowledge-roles
description: Use for Tau agent roles and role groups, model selection, tool policy, runtime overrides, delegation permissions, discovery opt-in, and self_info.
advertise: false
---

# Roles and effective permissions

Configure persistent roles in `~/.config/tau/harness.yaml` under
`agents.role_groups.<group>.roles.<role>`. Optional `main_priority` selects the
enabled role with the lowest effective nonnegative number (bot 10, coordinator
100). Zero is valid; an absent or null effective value means no candidate.
Overlays replace values. Winning ties fail config; nonwinning ties warn.
An unavailable winner fails without fallback. This role-only field does not
inherit defaults or exclude ordinary workers. Main's session-bound ID survives
restart/priority changes. UI demand defaults to main; explicit `--role` starts
ordinary work, and attachment selection wins. Without main, startup uses
role/group order. Retired `agents.default_role` and `inter_session.receiver`
keys are rejected.
Global `agents` settings apply first, then group defaults,
then role settings. Role names are globally unique; a role can set its model,
effort, prompts, required skills, and tool policy. The effective tool surface
also depends on extension defaults and harness `tool_policy.rules` before role
overrides. A role's `tools` is an explicit allow-list base when present;
`disable_tool_tags`, `enable_tool_tags`, `disable_tool_groups`,
`enable_tool_groups`, `disable_tools`, then `enable_tools` apply in that order.
For example, disabling the `compaction` group removes the `compact` tool,
whereas `inference_compaction: disabled` only changes provider-inline and
reactive-overflow behavior, not the named standalone policies or the tool.
Cross-agent compaction is separately opt-in.

Roles that need `session_list` or `agent_list` must enable
`session_discovery` or `agent_discovery` respectively; the lists alone
grant no messaging or watch authority. Use `tau --role NAME dev print-tools`
to preview one fresh agent's tool surface and `tau --role NAME dev print-prompt`
to preview its prompt. These start extensions with normal state/external side
effects and do not describe running/restored agents. See `docs/agent-roles.md` for fields and
`tau-self-knowledge-prompt-templating` for fragment configuration.

In the terminal, `:role <role>` selects a role and `:new <role>` stages one
for a new agent. `:role <role> model ...` or `:role <role> enable-tool-groups
calendar,email` edits a role for **this run only**; `reset` removes a runtime
field override. `:model <provider>/<model>` changes the selected agent's
model without changing its role; `:effort 0.7` changes its effort for later
prompts. With no selected agent after `:new`, model and effort commands stage
one-shot next-agent overrides. They do not change a running prompt or persist
as role configuration. Edit `harness.yaml` for changes that must survive a
restart. Call `self_info({})` for authoritative current agent/session IDs,
model, effective effort, status, and available usage estimates rather than
inferring identity from a role name or a preview.

`self_info` also returns a snapshot of `Ready` extensions with their configured
names, instance IDs, `restart_attempt` values, and self-reported package releases
and optional build revisions. Missing metadata appears as `unknown`. The snapshot
omits disconnected or replaced instances. Use it for diagnostics, not build
attestation.
