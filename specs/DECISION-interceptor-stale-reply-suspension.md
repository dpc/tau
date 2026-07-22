# DECISION-interceptor-stale-reply-suspension: Consume one stale reply after destructive cancellation

Authority: confirmed, 2026-07-22, dpc

When agent unload, session rollover, or canceled peer receive destructively
cancels a publication whose interceptor request was already delivered, the
harness retains that interceptor registration but temporarily suspends its
connection from selection. Matching new publications bypass every registration
on the suspended connection.

The extension may replace its registration while suspended; the replacement is
stored and remains suspended. The next `InterceptReply` from that connection is
consumed without applying its action to any publication and clears the
suspension. Exactly one reply is consumed. Normal interception then resumes with
the current registration.

Suspension has no timeout. If the extension never replies, its registrations
remain bypassed for the rest of that connection. Disconnect clears the
suspension together with the disconnected registrations; a later connection and
registration start unsuspended.

This preserves the extension process and registration lifecycle while ensuring
that an uncorrelated reply for canceled work cannot bind to a later publication.
It also avoids permanently retiring a live interceptor merely because
harness-owned lifecycle cleanup canceled one request.

The harness architecture, event-processing contract, agent-message cleanup,
compaction cleanup, peer-routing cleanup, and security notes must remain
synchronized with this behavior:

- [ARCH-tau-harness](../crates/tau-harness/specs/ARCH-tau-harness.md)
- [SPEC-tau-harness-event-processing](../crates/tau-harness/specs/SPEC-tau-harness-event-processing.md)
- [SPEC-agent-message-delivery](SPEC-agent-message-delivery.md)
- [SPEC-compaction-and-context-recovery](SPEC-compaction-and-context-recovery.md)
- [SPEC-tau-harness-peer-routing](../crates/tau-harness/specs/SPEC-tau-harness-peer-routing.md)
- [SECURITY](../SECURITY.md)

This decision was explicitly confirmed with the reply `approved` after the full
contract above was presented. It satisfies
[DECISION-persistence-and-extension-interface-change-approval](DECISION-persistence-and-extension-interface-change-approval.md).
