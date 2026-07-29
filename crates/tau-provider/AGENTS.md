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
