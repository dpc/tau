# SPEC-extension-notice-requests: Extension notice requests

## Record justification

Extension notices span shared protocol messages, client helpers, extension
activation and metering, harness authority and interception, debug projection,
and UI delivery, so no one local artifact can own the complete contract.

An authenticated configured extension requests a routine user-visible notice
with `extension_notice_request(message, level)`. The request is not an event and
contains no target, persistence, provenance, visibility, correlation, or result
authority. Every configured extension kind may send it during handshaking or
ready operation without a capability bit. Unconfigured, disconnected, socket,
and external peers have no authority.

Before activation completes, the request retains wire order in the existing
bounded operational queue. Once eligible, the harness handles it inline without
interception, event commit, broadcast, semantic persistence, replay, or a result
message. It publishes a separate live-only `harness.notice` through ordinary
interception and broadcast with:

- `kind = "extension.notice"`;
- the exact requested message;
- the requested level, capped from critical to warning; and
- `purpose = "diagnostic"`.

The harness owns the output source and publication metadata. Interceptors may
drop it or replace only its message under ordinary notice policy. It creates no
current-state snapshot and is never replayed. Debug logging and protocol
metering may observe the input and resulting output separately.

Generic peer event emission never authorizes extension-authored
`harness.notice`. `ConfigError` remains a separate mandatory startup and
lifecycle alert path and must not be used for routine notices. Extensions cannot
promote routine requests to `response` or `alert`.
