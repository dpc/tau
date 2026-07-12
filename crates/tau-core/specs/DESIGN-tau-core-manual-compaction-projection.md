# DESIGN-tau-core-manual-compaction-projection: Manual compaction projection

Status: unconfirmed

The agent-tree fold validates unique bounded manual request ids, immutable
caller/target/model/tool-call correlation, and exactly one pre-start outcome.
It projects waiting, started (including transaction outcome), and failed state
so the harness can repair every crash window without resending ambiguous
provider work or duplicating a background completion.
