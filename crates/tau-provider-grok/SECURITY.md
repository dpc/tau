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
