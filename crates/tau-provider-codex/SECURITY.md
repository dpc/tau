# Codex provider security boundary

## OAuth responses and diagnostics

OAuth endpoints and intermediaries are untrusted external ingress. The Codex
OAuth client bounds every token response before parsing, checks the final
decoded byte length again, and never retains a raw failure body. Bounded
`provider_code()` and `message()` fields remain untrusted provider content: an
endpoint can reflect a submitted authorization code, verifier, access token, or
refresh token, so callers must never log those accessors. `OAuthError` keeps
default `Display` and `Debug` provider-content-free except for safe HTTP status
and a closed allowlist of fixed provider codes. Provider-builtin owns the
generation cache and locked-profile fallback policy; this crate never persists
negative authentication state.

Authenticated account quota is external-provider data governed by
[DECISION-provider-quota-pacing](../../specs/DECISION-provider-quota-pacing.md).
`/wham/usage` requests disable redirects, use a short timeout and body cap, and
never expose bearer/account headers or raw response bodies above this crate.
Pool ids use bounded normalized keys; oversized or colliding full snapshots
fail atomically. Missing/default/sole pools in account snapshots never prove
model applicability: only a supported in-band transport observation may carry
a route binding. The official WebSocket contract defines a valid nameless
`codex.rate_limits` turn event as the canonical default `codex` pool; a present
non-null malformed pool id in either optional field is rejected rather than
ignored or reinterpreted as that default. JSON null is treated as absence.

## WebSocket liveness and cancellation

OAuth, quota, compact, and WebSocket upgrade traffic uses the shared immutable
outbound policy in `tau-provider`. Reqwest does not rediscover environment
proxies or follow redirects. A selected proxy route cannot fall back direct;
platform verification plus the optional startup custom CA applies to target
and HTTPS-proxy TLS. Upgrade and transport errors retain only bounded status
and route/phase facts, never endpoints, credentials, CA material, or raw
failure bodies.

Fresh DNS/TCP/TLS/WebSocket upgrades race the prompt abort registry and a
30-second deadline. Cancellation is rechecked after waker registration and after
upgrade success. Canceled work returns the typed cancellation outcome; timeout is
a redacted outbound `Deadline` during `Connect`, classified as retryable
`Transport`. Failure, timeout, and cancellation all abandon the same-key pool
reservation.

The five-minute provider-frame idle watchdog begins only after a successful
upgrade and request send; it is not the connection deadline. Revisit both bounds
when upstream handshakes, proxy behavior, or cancellation ownership changes.

Best-effort prewarm runs as provider-supervised work rather than on the event
loop. It has at most a 30-second upgrade plus a 30-second absolute response
wait, observes cancel/shutdown/profile invalidation through the transport abort
waker, and cannot reinstall a socket after cancellation or invalidation.
# Provider VCR boundary

Raw provider captures are private sensitive test artifacts and may contain
prompts, credentials, identifiers, reasoning, tool output, or host paths. They
must never be copied directly into the public fixture corpus. Public cassettes
are synthetic, structurally allowlisted, bounded, and require exact terminal
and frame consumption through replay-only production loading with no live
fallback. Fixture publication or replay/capture changes require independent
privacy review.
