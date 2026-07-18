# tau-provider

- Read the repository root `AGENTS.md` before making changes.
- Read `SECURITY.md` before changing the shared environment-aware HTTP agent,
  proxy selection, TLS roots, or response decoding features.
- Revisit both pre/post-decode response bounds and add compressed-expansion
  tests before enabling or feature-unifying ureq gzip, brotli, or charset
  decoding.
