# DECISION-extension-tool-prefixes: Per-instance structural tool prefixes

Authority: confirmed, 2026-07-13, dpc

## Decision

Each configured extension instance may receive one immutable, narrowly
structural tool prefix before declarations and readiness. The prefix qualifies
internal names, model-visible aliases, and groups, but never rewrites tags,
actions, prompt prose, schemas, grammars, examples, or other extension meaning.

Final internal-name collisions are rejected deterministically; an incumbent is
not evicted, and an effective prompt containing a visible collision is rejected.
Durable facts use the final name without retroactive rewriting.

This keeps instance configuration harness-owned while declarations and semantics
remain extension-owned. Narrow qualification avoids ambiguous semantic rewrites;
collision rejection costs explicit disambiguation. Exact behavior is specified by
[SPEC-tau-harness-extension-lifecycle](../crates/tau-harness/specs/SPEC-tau-harness-extension-lifecycle.md)
and [SPEC-tau-harness-prompt-dispatch](../crates/tau-harness/specs/SPEC-tau-harness-prompt-dispatch.md).
