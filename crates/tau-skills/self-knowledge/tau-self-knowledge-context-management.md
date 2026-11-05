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

`when.at: outer_turn_starting` compacts before a new turn's first inference,
not same-turn continuations or merely queued work. Status filters use the last
runtime status, lost on reload; without a visible status tool this checkpoint
matches `working`. It coalesces with `before_inference` at the lowest threshold.
Keep a `before_inference` safety rule for mid-turn pressure.

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

Input usage **strictly above** the threshold queues a notice after the response
and its tools. Alerts also support `outer_turn_finished`. Each fires once until
usage falls to/below threshold or accounting resets. Restart loses queued alert
state, not committed deliveries. Alerts do not grant disabled tools.
Override named policies/alerts at group or role scope.

See `docs/agent-roles.md` for the full merge rules, accepted conditions,
template variables, and validation; `specs/SPEC-compaction-and-context-recovery.md`
owns the compaction/replay contract.
