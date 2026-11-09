# SPEC-discovery-context-presentation: Discovery context presentation

## Record justification

Discovery presentation spans extension scanning, harness delivery, context history, and shared envelope rendering, so no single implementation artifact owns the complete contract.

## Purpose

Discovery gives the agent applicable AGENTS.md instructions and available skill descriptions without repeating information already in its surviving selected context. Its presentation follows [SPEC-context-output-formatting](../crates/tau-harness/specs/SPEC-context-output-formatting.md).

## Delivery

Startup and workdir changes use the same compact presentation. Scanning happens eagerly; model-context delivery is lazy and passive, accompanying an already-authorized inference rather than starting a turn of its own. Initial discovery accompanies the first inference as separate user-role context, not text inserted into the user's prompt.

Delivery considers items from startup and all previous workdir discoveries in the agent's surviving selected context, not just the previous directory. Unchanged items are omitted, including when returning to a directory. Changed items are delivered in full with an updated marker, not as diffs, relative to their latest delivered content.

No new or changed items means no injection. Removal alone does not produce a notice.

Discovery readiness and delivery-history handling follow [SPEC-session-discovery-declarations-and-readiness](SPEC-session-discovery-declarations-and-readiness.md).

## AGENTS.md instructions

Each file has its own `<agents_md path="…">` envelope containing its full applicable instruction text. A changed, previously delivered file also has `updated="true"`.

## Skills

Skill advertisements share one `<skills>` envelope per delivery. Each entry uses the form ``* `skill-name` - description``. An updated entry uses ``* `skill-name` (updated) - description`` with the current description.

Discovery advertises skills; it does not load their full bodies.

## Information and concision

The envelopes carry the instructions or skill entries directly. There is no extra discovery wrapper, repeated catalog, scope summary, visible version or hash, or bookkeeping prose.
