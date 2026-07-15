# Shared provider security boundary

## OAuth responses and diagnostics

OAuth endpoints and intermediaries are untrusted external ingress. The shared
OAuth client bounds every token response before parsing, checks the final
decoded byte length again, and never retains a raw failure body. Typed errors
may retain bounded, single-line provider code/message fields for control flow,
but those fields are not credential-safe: an endpoint can reflect a submitted
authorization code, verifier, access token, or refresh token in otherwise
recognized JSON.

`OAuthError` therefore keeps `Display` and `Debug` provider-content-free except
for safe HTTP status and a closed allowlist of fixed provider codes. Callers may
log those default projections, but must not trace the raw field accessors.
Response-size, nested/flat envelope, malformed-body, truncation, and safe
formatting regressions are owned by this crate.

The shared permanent-refresh classifier recognizes only fixed
credential-invalidating codes on HTTP 400/401. Provider-builtin owns the
generation cache and locked-profile fallback policy; this crate never persists
negative authentication state.

Workspace `ureq` currently disables gzip, brotli, and charset decoding. Enabling
or feature-unifying any response decoding requires revisiting the pre/post
decode bounds here and adding compressed-expansion coverage before preserving
the same safety claim.
