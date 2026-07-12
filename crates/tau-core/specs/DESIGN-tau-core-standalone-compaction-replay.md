# DESIGN-tau-core-standalone-compaction-replay: Durable standalone-compaction replay

Status: confirmed, 2026-07-11, user

A continuation inference checkpoint is accepted for a successful standalone
transaction only when its provider-qualified model equals the start model, its
operation is inference, and its activation cut equals the start cut. Ordinary
new-format checkpoints likewise carry the model/operation/cut correlation as
one complete tuple; legacy all-absent tuples remain replay-compatible but are
not eligible to substitute current model ownership.

New-format compaction boundaries carry one all-present metadata group:
transaction id, cut, suffix end, compact prompt id, provider-qualified model,
and operation. Replay resolves the transaction's Started fact; requires exact
cut/prompt/model/operation correlation and standalone operation; requires
`suffix_end` to equal the boundary parent with cut as its ancestor; and rejects
partial groups, unknown transactions, mismatches, and duplicate outcomes.
Live validation and replay use the same fail-closed rules. Historical
all-six-absent boundaries remain valid hard boundaries but cannot participate
in new transaction recovery.
