# DECISION-extension-tool-prefixes: Per-instance structural tool prefixes

Authority: confirmed, 2026-07-13, dpc

Each configured extension instance may receive one immutable, narrowly
structural tool prefix before declarations and readiness. The prefix qualifies
internal names, model-visible aliases, and groups, but never rewrites tags,
actions, prompt prose, schemas, grammars, examples, or other extension meaning.
The harness validates the assigned envelope and final-name ownership but never
rewrites extension events.

Final internal-name collisions are rejected deterministically; an incumbent is
not evicted by respawn or later registration. Policy may keep duplicate visible
aliases mutually exclusive, but an effective prompt containing a collision is
rejected. Durable dispatch, completion, replay, history, and UI use the final
name already present in protocol facts without retroactive rewriting.

This keeps instance configuration harness-owned while declarations and semantics
remain extension-owned. Narrow qualification avoids ambiguous schema/prose
rewrites; collision rejection preserves deterministic dispatch at the cost of
requiring configuration or policy to disambiguate competing surfaces.

The Configure, registration, startup-preflight, refresh, policy, and persistence
contracts are specified by
[SPEC-tau-harness-extension-lifecycle](../crates/tau-harness/specs/SPEC-tau-harness-extension-lifecycle.md)
and [SPEC-tau-harness-prompt-dispatch](../crates/tau-harness/specs/SPEC-tau-harness-prompt-dispatch.md).
This interface is governed by
[DECISION-persistence-and-extension-interface-change-approval](DECISION-persistence-and-extension-interface-change-approval.md).
