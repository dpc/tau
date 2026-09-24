# Grok protocol boundary

This is an async library intended for Tau's provider extension, not an independent
credential owner. See [README.md](README.md) for implemented scope and remaining
integration; the [root policy](../../SECURITY.md) owns the harness/extension
boundary.

OAuth functions use the caller's Tokio runtime. Production requests go only to
the fixed HTTPS xAI issuer through Tau's frozen outbound policy. Redirects and
transparent retries are disabled. Server response bodies are untrusted, bounded,
and absent from error formatting. Credential containers deliberately have no
`Debug`. Verification URLs are displayed for the user; the library never opens
a browser or forwards credentials to that URL.

Device polling is required by the external RFC 8628 protocol. Local cancellation
is notification-driven through a caller-supplied future; poll waits and active
exchanges both respect cancellation and the grant deadline.

The caller must serialize refreshes and save rotated credentials atomically
before publishing a replacement generation. An ambiguous refresh transport
failure can mean the old token was consumed; this library makes no automatic
retry and provides no reuse/grace guarantee. Remote `userinfo` supplies subject
identity; account pinning is the caller's responsibility. Dropping a future
cancels local waiting, not a server-side grant or charge.

No function accesses Secret storage, another application's credentials, cookies,
billing settings, or remote revocation. Tests use only synthetic credentials and
loopback HTTP or controlled clocks. Authentication-origin changes, retry changes,
or credential-storage integration require revisiting these boundaries and their
focused tests.

Model discovery uses the fixed public `https://api.x.ai/v1/models` endpoint with
one caller-supplied bearer generation, a 30-second deadline, no redirects or
transparent retries, and a 1 MiB decoded body limit. It stores no credentials,
performs no refresh, and exposes only closed failure categories. A listed model
does not establish subscription entitlement, free usage, native tool-image
support or a text-inference route.

Request lowering consumes an already destination-projected prompt under
[`REQ-best-effort-provider-switching`](../../specs/REQ-best-effort-provider-switching.md).
It grants no new origin admission and preserves admitted raw JSON rather than
repairing foreign reasoning. Owned tool outputs use the canonical rendered
text and typed validated image bytes; images remain tool content, never a new
user-role payload. Canonical high detail follows
[`SPEC-typed-image-tool-results`](../../specs/SPEC-typed-image-tool-results.md).
Failed tool results do not forward images. Shared request
bounds prevent repeated tool results from multiplying each image allowance.
No request method sends inference traffic. Runtime integration must retain
matching shared-attempt attribution and tool policy. Grok prepared requests
select exact `max_prompt_tokens` and `max_time_limit` incomplete reasons as
nonretryable Error terminals retaining only validated assistant prose, terminal
usage and response identity. They strip every tool call and opaque item and
grant no output-length continuation or context-overflow recovery.
