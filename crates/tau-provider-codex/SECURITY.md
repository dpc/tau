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
Refresh alone uses the upstream JSON request contract. Successful refresh
responses may omit access and refresh replacements; callers preserve those
credential fields. A supplied access replacement must be a non-expired JWT and
its `exp` claim, rather than provider-relative lifetime metadata, determines
local expiry. Authorization-code exchange remains form encoded.

Authenticated account quota is external-provider data governed by
[GATE-provider-quota-pacing](../../specs/GATE-provider-quota-pacing.md).
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

Incoming provider data crosses the external-provider trust boundary. Tau owns
1 MiB frame and complete-message limits, queues one complete raw event, and lets
queue saturation backpressure the socket without loss or reordering. The turn
owner parses each event once and enforces separate 64 MiB cumulative-attempt
text and logical retained-state admission budgets. Discarded transparent-repair
bytes remain charged. Equality is accepted; the first excess fails before
semantic mutation with a fixed content-free terminal invalid-response error and
retires the socket, so deterministic oversized work is neither repaired nor
retried.

Cancellation and local writer failure never join the provider-data FIFO. They
share an independent coalesced constant-size wake state, and the turn owner
checks that state before processing queued provider data.

Best-effort prewarm runs as provider-supervised work rather than on the event
loop. It has at most a 30-second upgrade plus a 30-second absolute response
wait, observes cancel/shutdown/profile invalidation through the transport abort
waker, and cannot reinstall a socket after cancellation or invalidation.

Inference can spend at most one immediate WebSocket repair before semantic
assistant, reasoning, tool, or opaque output. Exact stale-chain and
connection-limit codes are trusted only from canonical typed envelopes; provider
prose cannot trigger repair. Received-byte accounting is cumulative across the
repair, while tentative semantic output causes the error to surface without an
automatic replay. The extension clears that transient output before scheduling
later required work.

Ordinary response ids are socket-local and carry no independent proof that
concurrently committed canonical input exists in their upstream history. Tau
reuses one only when a type-preserving fingerprint proves the current canonical
prefix through that response is exact. A mismatch removes the id and full-replays
context on the same socket; only a successful response publishes a new anchor.

Prewarm response ids are usable only with the same socket, opaque
profile/mode/cache identity, exact lowered-request fingerprint, and prefix
continuation. Cancellation and invalidation generations are rechecked at socket
publication so stale work cannot reinstall credential-bearing transport state.
# Provider VCR boundary

Raw provider captures are private sensitive test artifacts and may contain
prompts, credentials, identifiers, reasoning, tool output, or host paths. They
are stored as private `.json.zst` diagnostics but compression does not redact
or reduce their sensitivity. They must never be copied directly into the public fixture corpus. Public cassettes
are synthetic, structurally allowlisted, bounded, and require exact terminal
and frame consumption through replay-only production loading with no live
fallback. Fixture publication or replay/capture changes require independent
privacy review.

Failed finite Responses attempts also write a schema-v1
`responses-attempt-failure` sidecar when provider capture is enabled. The
sidecar omits request bodies, headers, endpoints, credentials, model output,
provider prose, close-reason prose, raw JSON values, and raw library errors. It
retains bounded structural shape, validated provider codes and identifiers,
and message/reason lengths, so it remains potentially credential-bearing and
must receive the same private handling as full provider captures. Ordinary live
retry status may contain separately bounded, single-line, known-secret-scrubbed
provider detail and is therefore also potentially sensitive.
