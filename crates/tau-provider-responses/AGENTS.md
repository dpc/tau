# tau-provider-responses

Read the repository root `AGENTS.md`, `specs/ARCH-tau-provider-responses.md`,
`SECURITY.md`, and the applicable root specs before changing public Responses request
construction, transcript replay, streaming, or transport behavior.

- Before introducing a new free-form payload kind, or reframing an existing one, in the shared generic user-role `ContentPart::Text` carrier, read [`GATE-new-generic-user-payload-envelopes`](../../specs/GATE-new-generic-user-payload-envelopes.md) and [`SPEC-exact-sentinel-prompt-envelopes`](../../specs/SPEC-exact-sentinel-prompt-envelopes.md); use the shared registry rather than a component-local provenance wrapper. Typed tool results and the system/developer prompt channel are outside that rule.
