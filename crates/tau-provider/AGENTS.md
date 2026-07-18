# tau-provider

- Read the repository root `AGENTS.md` before making changes.
- Read `SECURITY.md` before changing the shared asynchronous HTTP policy,
  proxy selection, TLS roots, or response decoding features.
- Keep `specs/ARCH-tau-provider.md` synchronized with outbound-policy ownership
  and supported routing behavior.
- Revisit both pre/post-decode response bounds before enabling response
  decompression or charset conversion.
