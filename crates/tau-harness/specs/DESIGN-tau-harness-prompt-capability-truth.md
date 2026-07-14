# DESIGN-tau-harness-prompt-capability-truth: Prompt capability truth

Status: unconfirmed

Prompt templates receive sparse capability data owned by the harness. For each
turn the harness resolves the concrete agent role/model and one effective tool
snapshot after policy and provider-supported-type filtering. That snapshot is
the source for tool definitions, authorization, tool fragments, and
`capabilities.tools.available`; non-tool extension side queries intentionally
receive an empty available list because local authorization forbids calls even
though wire definitions remain cache-compatible. Extension enabled/Ready state
is captured at render time. Render failures are explicit and prevent provider
dispatch; capability state is not persisted separately.

The same concrete model snapshot supplies
`capabilities.tools.parallel_calls`. Prompts advertise parallel execution only
when the effective provider route publishes support; otherwise they state the
one-call-per-response limit. Parsing, persistence, and dispatch remain lossless
if a provider violates a false declaration and returns multiple calls.
