# tau-provider

- Read the repository root `AGENTS.md` before making changes.
- Read `SECURITY.md` before changing the shared asynchronous HTTP policy,
  proxy selection, TLS roots, response decoding features, or provider debug
  capture validation, compression, queueing, and filesystem writes.
- Keep `specs/ARCH-tau-provider.md` synchronized with outbound-policy ownership
  and supported routing/capture behavior.
- Revisit both pre/post-decode response bounds before enabling response
  decompression or charset conversion.
- Provider debug captures are sensitive and best-effort. Preserve typed
  session/prompt paths, the shared filename contract, nonblocking bounded
  admission, off-path compression/I/O, and explicit failure/shutdown semantics.
  Keep the grammar owner and test ownership in `docs/testing.md` synchronized.
  Capture changes require focused security and independent privacy review.

- Before introducing a new free-form payload kind, or reframing an existing one, in the shared generic user-role `ContentPart::Text` carrier, read [`GATE-new-generic-user-payload-envelopes`](../../specs/GATE-new-generic-user-payload-envelopes.md) and [`SPEC-exact-sentinel-prompt-envelopes`](../../specs/SPEC-exact-sentinel-prompt-envelopes.md); use the shared registry rather than a component-local provenance wrapper. Typed tool results and the system/developer prompt channel are outside that rule.
