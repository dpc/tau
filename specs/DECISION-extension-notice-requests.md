# DECISION-extension-notice-requests: Request routine extension notices through a dedicated message

Authority: confirmed, 2026-07-21, dpc

Authenticated configured extensions request user-visible routine notices with a
dedicated `extension_notice_request` input message. They do not publish
`harness.notice` events. The request payload contains exactly a human-readable
`message` and a `NoticeLevel`; it contains no event kind, target, transience,
`always_show` flag, publisher or provenance claim, correlation id, or result
address.

Every authenticated configured extension entry kind, including configured Core,
may send the request without a capability bit. It is legal in `Handshaking` or
`Ready`, including after `Ready` is received while the global activation barrier
remains pending. An out-of-phase request from a configured extension follows the
normal extension protocol-failure policy. A claimed `Hello` kind, unconfigured or
disconnected connection, socket UI, and external peer grant no authority; after
ordinary decoding and any applicable metering, their requests are silently denied
without an output, result, diagnostic, or disconnect. Peer-authored `Emit(harness.notice)` is
denied by default event admission. `Event::HarnessNotice` remains a
harness-to-subscriber output event.

The request is operational traffic. Before the extension and global activation
barriers release, the complete request retains wire order in the existing bounded
activation queue and consumes the existing per-connection frame and byte quotas.
It is not a startup declaration and gains no bootstrap exception. Disconnect before
activation release drops the retained request. Once eligible, the harness handles
it inline as a protocol request; the request is not intercepted, committed as an
event, broadcast, persisted, or replayed. Disconnect of the origin after inline
handling does not cancel the separate harness-authored publication.

Handling creates a separate harness-authored, live-only `harness.notice` output
with these fixed fields:

- `kind = "extension.notice"`;
- `message` copied exactly from the request;
- requested `level`, except `critical` is capped to `warning`;
- `always_show = false`.

The output carries no semantic extension provenance and uses the harness as its
publication source. It traverses ordinary event interception, runtime commit and
broadcast to every currently matching subscriber. Interceptors retain the
established non-mandatory notice behavior: they may drop the output, and a
replacement may change only `message`; `kind`, `level`, `always_show`, transient
metadata, and publication source remain unchanged. The originating extension
receives the output too if its current subscription matches;
this decision adds no origin suppression, feedback-loop detection, coalescing, or
rate limit.

The output is transient live state. It enters no agent, session, restore, or other
semantic journal, creates no current-state snapshot, and is never replayed to a
late subscriber. A successful client-side send or write confirms only local
protocol submission; there is no result message or proof that interception allowed
the notice to reach a UI. A crash or disconnect may leave a submitted request without
visible output, and no recovery retries it.

The dedicated request and resulting output retain ordinary trusted-local resource
and diagnostic accounting. The existing decoded-input frame limit applies to the
request; no smaller notice-specific payload or rate limit is introduced. Pre-activation
requests additionally consume the existing activation quotas. Protocol I/O
metering records the uplink as `message.extension_notice_request` and delivered
output as `harness.notice`. Under the existing redaction and string-compaction
policy, debug JSONL records the dedicated input frame and, when interception permits
commit, the separate published output. Debug traces are not replay or semantic
persistence.

`ConfigError` remains a separate protocol and lifecycle path. It may reject
startup, bounds and surfaces extension configuration failures as mandatory
replayable diagnostics, and is not a routine notice request. Implementations must
not route `extension_notice_request` through the mandatory/replayable configuration
or harness-warning helpers.

This replaces the legacy sanitizer that accepted extension-authored
`Emit(harness.notice)`. First-party extension producers must send the dedicated
request. There is no compatibility shim or dual-authority period under
[DECISION-no-backward-compatibility](DECISION-no-backward-compatibility.md).
The authority matrix and implementation records governed by
[DECISION-generic-peer-event-emission](DECISION-generic-peer-event-emission.md)
must record the dedicated request and preserve `harness.notice` as a harness-owned
output fact.
