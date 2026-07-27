# SPEC-extension-tool-prefixes: Per-instance structural tool prefixes

## Record justification

Tool prefixes span configuration inheritance, client declaration builders,
extension activation, harness registration ownership, prompt tool projection,
dispatch, and durable facts, so no one local artifact can own the complete
contract.

Each configured extension instance may receive one immutable structural tool
prefix before declarations and readiness. The prefix qualifies internal names,
model-visible aliases, and groups. It never rewrites tags, actions, prompt prose,
schemas, grammars, examples, or other extension meaning.

Final internal-name collisions are rejected deterministically; an incumbent is
not evicted. Effective prompts reject simultaneously visible alias collisions.
Durable facts retain the final names without retroactive rewriting.

Configuration remains harness-owned while declarations and semantics remain
extension-owned. Changing an assigned prefix requires restarting that extension
instance.
