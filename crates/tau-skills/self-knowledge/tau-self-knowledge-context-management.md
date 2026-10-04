---
name: tau-self-knowledge-context-management
description: Use for Tau context windows, automatic or manual compaction, compaction policy thresholds and reserves, provider-inline recovery, and context-size alerts.
advertise: false
---

# Managing agent context

Set named policies in `harness.yaml` under `agents.compactions`, or override
them within a role group or role. This adds a task-boundary policy without
replacing Tau's built-in `default` policy (`before_inference`,
`threshold: context_limit_safe`):

```yaml
agents:
  compactions:
    after-task:
      reserve: 40000
      when:
        at: outer_turn_finished
        statuses: [done]
```

`reserve: N` means the selected model's effective input limit minus N tokens.
Use either `reserve` or `threshold`, not both. `threshold: provider_default`
uses the model's published standalone threshold; `context_limit_safe` uses the
adapter's safe scheduling threshold. Disable the built-in policy with
`compactions.default.enable: false`, not by adding another named policy.
Policies at one lifecycle point combine: the lowest matching threshold
wins, and at most one standalone compaction starts. Status conditions are
optional. With no `status` tool in the frozen prompt, open/settled turns
match `working`/`done` for this purpose.

Use `when.at: outer_turn_starting` for lazy compaction before the next outer
turn's first inference instead of eagerly after the previous finish. It never
runs on same-turn tool or other continuations. Queued/coalesced work qualifies
only when it actually begins a new turn. A `statuses: [done]` filter uses the
last runtime work status (not persisted across reload); without a visible status
tool this checkpoint matches `working`. At turn start it coalesces with matching
`before_inference` policies into one compaction at the lowest threshold. Keep
an independent `before_inference` safety rule for mid-turn context pressure.
Existing defaults and `outer_turn_finished` semantics are unchanged.

Manual UI `:compact`, the model's `compact` tool, optional same-session
`agent_compact` tool, and scheduled standalone compaction are separate
entrypoints. Standalone work prefers provider-native compaction when available;
other routes use Tau's local summary fallback where supported. The singular
`inference_compaction` role setting instead controls provider-inline compaction
and reactive recovery from a canonical no-output context overflow; disabling
it does **not** disable named standalone policies or the `compact` tool.
Neither a threshold nor a context alert guarantees a particular resulting
window size. Load `tau-self-knowledge-roles` for tool permissions.

For advice rather than automatic compaction, configure a named alert:

```yaml
agents:
  context_size_alerts:
    compact-soon:
      threshold: 160000
      when:
        at: after_response
      message: "Context is large. Finish the current task, then use the compact tool."
```

After a completed inference reports input usage **strictly above** its
threshold, an alert queues an internal notice after the response and its tool
calls. `outer_turn_finished` is another supported lifecycle point. An alert
fires once while above threshold during one running daemon, and rearms when
usage falls back to or below the threshold or accounting resets. Queued alert
state is not reconstructed after restart; committed delivery remains in
history. An alert does not grant a disabled compaction tool. Named policies
and alerts can be specialized at group/role scope.

See `docs/agent-roles.md` for the full merge rules, accepted conditions,
template variables, and validation; `specs/SPEC-compaction-and-context-recovery.md`
owns the compaction/replay contract.
