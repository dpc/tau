# ARCH-tau-ext-zulip: Zulip bridge architecture

The disabled-by-default `std-zulip` process is an extension-local adapter under [ARCH-external-message-boundary](../../../specs/ARCH-external-message-boundary.md) and [SPEC-external-message-reports-and-facts](../../../specs/SPEC-external-message-reports-and-facts.md). It declares `PeerCapability::MessageBridge`; no Zulip-specific protocol or harness state exists.

A registered agent creates a Zulip event queue using Basic bot auth on a bounded worker. One process worker long-polls increasing event IDs, ignores heartbeats and self events, applies exact numeric sender and conversation policy, then emits transient message reports. The harness authenticates and stamps the configured extension publisher before durable canonical facts and live wakeup. Exact route selection and runtime authority follow [SPEC-tau-ext-zulip-routing](SPEC-tau-ext-zulip-routing.md).

Zulip direct conversations derive from sorted participant IDs. Stream conversations derive from `(stream_id, topic)`, because topics are Zulip's native thread unit. Configured routes independently grant receive and proactive-send authority. Receive installs bounded source-bound reply/mutation authority only after local report submission. Tool calls resolve opaque references or configured aliases back to private native routes.

Queue identity, cursor, registrations, recent IDs, message ownership, and routes are bounded process-local state. Reconfiguration, unregister, agent unload, and shutdown advance generations and retire affected authority. Queue loss starts from a fresh live tip and reports a possible gap; it never invents backlog recovery or durable queue state.

Successful remote sends emit `message.sent_reported` before the terminal tool result. There is no remote/local transaction, durable outbox, automatic ambiguous retry, exactly-once guarantee, upload/download authority, or reconstruction of reply authority from replay.
