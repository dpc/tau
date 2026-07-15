# tau-provider

- Read the repository root `AGENTS.md` before making changes.
- Read `SECURITY.md` before changing OAuth HTTP, parsing, error, formatting, or
  logging behavior.
- Shared OAuth errors must keep credential-safe `Display` and `Debug`
  projections; bounded parsed provider fields remain untrusted and must not be
  logged directly.
- Revisit both pre/post-decode response bounds and add compressed-expansion
  tests before enabling or feature-unifying ureq gzip, brotli, or charset
  decoding.
