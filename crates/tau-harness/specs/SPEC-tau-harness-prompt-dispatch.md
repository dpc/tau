# SPEC-tau-harness-prompt-dispatch: Prompt Dispatch

## Prompt capability snapshot

Prompt capability conditionals use one turn-local snapshot. Tau resolves the
actual agent model, filters provider-supported and policy-effective tool specs,
and uses those same specs for provider definitions, authorization, fragments,
and template capabilities. Enabled configuration and Ready extension runtimes
are captured at that boundary. Later registration/restart changes affect only
later turns; raw capability context is not persisted. Non-tool extension side
queries are the intentional exception: provider definitions remain unchanged
for cache compatibility, while locally unauthorized tool capabilities and tool
fragments are empty.

## Provider model registry

Provider model snapshots are flattened in lexicographically sorted source-id
order, with the last advertisement for an exact provider-qualified model id
winning both metadata and routing. Duplicate ids produce an ordinary warning
whose displayed id count and per-id text are bounded; this diagnostic does not
change winner selection.

## Tool prompt-surface policy

Extensions and providers publish metadata only: tools declare neutral `ToolTag`s
(such as `shell:edit:line`, `shell:edit:apply_patch`, `shell:exec:generic`,
`shell:exec:shell_command`, and `shell:cd`) and providers publish model
`ModelTag`s (such as `shell:chatgpt`). The harness owns all matching policy.

Tool enablement starts from each extension's `enabled_by_default`, then matching
harness `tool_policy.rules` run deterministically by `(priority, rule name)`,
with each rule applying `disable_tool_tags` before `enable_tool_tags`. Built-in
and user policy share the same evaluator; the built-in `builtin.chatgpt-shell`
rule disables `shell:*` for ChatGPT-tagged models and re-enables apply-patch,
shell-command, cd, and directory-lock tools.

Role precedence is broad-to-specific and runs after global policy: optional
`tools` allow-list base, `disable_tool_tags`, `enable_tool_tags`,
`disable_tool_groups`, `enable_tool_groups`, `disable_tools`, then
`enable_tools`. This deliberately lets a role disable a broad family and
re-enable a narrower tag, group, or named tool.

Prompt dispatch snapshots the effective `ToolSpec` list for the selected prompt
model. Provider tool calls are validated against that prompt-owned snapshot, not
against mutable current role/model state after the user switches roles or models
mid-turn. Staged tool registration can never expand a prompt snapshot after it
was sent.

Narrow schema-guided argument repair also uses the prompt-owned `ToolSpec`.
Repair runs only after pre-dispatch validation failure, applies a small fixed set
of mechanical conversions, revalidates before dispatch, and falls back to the
normal rejection diagnostics when repair is unsupported or still invalid. Repair
traces are bounded metadata for logs/UI, not prompt-surface examples.

The loop guard is runtime-only per loaded agent branch. It records compact recent
assistant/tool-failure signatures, injects one hidden pivot prompt for obvious
cycles, and surfaces a mandatory notice instead of continuing automatically if the
same cycle persists. New user prompts and successful tool results reset detector
history and remove pending loop-guard pivots, but preserve unresolved in-flight
tool-call argument signatures for sibling calls in the same turn. Branch/head
moves invalidate the whole guard, including in-flight signatures, and remove
pending loop-guard pivots.

Provider-side `repetition_detected` final responses feed this same lifecycle with
a fixed harness-authored reason: first occurrence queues the pivot, recurrence
after that pivot stops automatic continuation. Provider error text is displayed
but is not trusted as model-visible guard instruction.

## Prompt dispatch lifecycle split

Prompt dispatch emits a lightweight transient `agent.prompt_started` lifecycle
fact immediately before the full `agent.prompt_created` provider work request.
Providers consume `agent.prompt_created`; UIs and side-effect observers should
subscribe to `agent.prompt_started` so materialized prompt context and tool
schemas are not sent over UI/control channels unnecessarily.

## Transient reply presentation

Durable envelopes retain source-owned `reply_path` for audit. Prompt assembly does not mutate that fact; it separately projects `reply` only when the route belongs to the target agent and the internally identified tool remains in the effective prompt snapshot, then uses its model-visible alias.

Compaction-triggered dispatch and continuation refine [SPEC-compaction-and-context-recovery](../../../specs/SPEC-compaction-and-context-recovery.md); that record owns their transaction, checkpoint, replay, and model-correlation behavior.

## Prompt capability trust boundary

Prompt capability data is sparse. Tool names are only the model-visible,
policy-authorized, provider-supported names advertised for that prompt; internal
aliases and registered-but-hidden tools are not exposed. It contains no
commands, secrets, failure text, or disabled extension catalog. Extension
`active` means protocol Ready, not feature health or sandboxing.
