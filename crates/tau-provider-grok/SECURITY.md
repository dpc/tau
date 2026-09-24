# Grok protocol boundary

This is a protocol and credential-policy library intended for Tau's provider
extension, not an independent Secret-store owner. See [README.md](README.md) for implemented scope and remaining
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

The credential module supplies no filesystem or cross-process ownership mechanism.
It follows authoritative read, one refresh, exact-byte CAS, and authoritative
reload through caller-supplied Secret callbacks. Runtime coalesces the same
generation within one process, following the existing Codex baseline; CAS
arbitrates saved generations, not remote token exchanges. Implementations comply with
[`GATE-extension-filesystem-mediation`](../../specs/GATE-extension-filesystem-mediation.md);
a writable extension state mount is not authority for direct operational I/O.

Integrations must bound/coalesce workers and check sticky cancellation before
refreshing. Once exchange begins the worker must stay alive through publication,
even if its waiter cancels. No rotated credential is published to inference
before an acknowledged durable CAS and authoritative reload. CAS conflict,
missing records, and account replacement never authorize recreating deleted
credentials. Storage errors are closed categories; timed-out submitted writes
may still commit. A post-CAS read must not race that mutation through another
connection. Even visible replacement bytes after a failed/lost acknowledgement
do not prove durability: the current admission fails conservatively.

An ambiguous refresh transport
failure can mean the old token was consumed; this library makes no automatic
retry and provides no reuse/grace guarantee. Remote `userinfo` supplies subject
identity at login; refresh retains the OAuth grant's account binding and rejects
adoption from another stored subject. Caller generation rejection must prevent
automatic reuse after a failed/ambiguous exchange or publication. Concurrent
processes can still exchange the same refresh token, and a crash before saving
can lose a rotated credential; either may require logging in again. This is not
an exactly-once or crash-recovery guarantee, and there is no durable refresh claim.
Dropping a future
cancels local waiting, not a server-side grant or charge.

No function directly accesses Secret storage, another application's credentials, cookies,
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
