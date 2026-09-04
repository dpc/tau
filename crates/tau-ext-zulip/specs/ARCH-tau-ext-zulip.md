# ARCH-tau-ext-zulip: Zulip bridge architecture

The disabled-by-default `std-zulip` process is an extension-local adapter under [ARCH-external-message-boundary](../../../specs/ARCH-external-message-boundary.md) and [SPEC-external-message-reports-and-facts](../../../specs/SPEC-external-message-reports-and-facts.md). It declares `PeerCapability::MessageBridge`; no Zulip-specific protocol or harness state exists.

In ordinary mode, a registered agent creates a Zulip event queue using Basic bot auth on a bounded worker. One process worker long-polls increasing event IDs, ignores heartbeats and self events, applies exact numeric sender and conversation policy, then emits transient message reports. The harness authenticates and stamps the configured extension publisher before durable canonical facts and live wakeup. Exact route selection and runtime authority follow [SPEC-tau-ext-zulip-routing](SPEC-tau-ext-zulip-routing.md).

Opt-in non-allowlisted activity collection keeps bounded process-local counts
only for stream creates that fail solely the numeric sender allowlist. It
retains no rejected body or raw topic. The next admitted message in the exact
same native stream/topic can carry one explicitly non-authoritative
bridge-authored activity note before its unchanged Markdown in the same
external-content report and wake. This mixed-provenance representation is the
approved Zulip exception documented by
[SPEC-external-message-reports-and-facts](../../../specs/SPEC-external-message-reports-and-facts.md).
The state is best effort: it expires, clears with queue and authority epochs,
and is lost on process restart.

Opt-in `send_only: true` instead declares only the send tool, without a tool group, and grants one fixed proactive direct-message route. It forbids sender allowlists, aliases, direct-message or stream receive routes, catch-up, replies, reactions, queue registration, polling, Zulip-originated event publication, and agent activation. The send tool uses the configured destination alias without listener registration; the caller never supplies a recipient. Changing modes requires process restart so startup tool declarations cannot diverge from runtime authority. The user explicitly approved these extension-interface semantics for the dedicated Nix/SSH host-monitor role, satisfying [GATE-persistence-and-extension-interface-change-approval](../../../specs/GATE-persistence-and-extension-interface-change-approval.md).

Zulip direct conversations derive from sorted participant IDs. Configured stream names resolve to native IDs before each queue registration, and stream conversations then derive from `(stream_id, topic)`, because topics are Zulip's native thread unit. Each `all_messages` route subscribes the bot before queue registration; removal never unsubscribes. Configured routes independently grant receive and proactive-send authority. Receive installs bounded source-bound reply/mutation authority only after local report submission. Tool calls resolve opaque references or configured names back to private native routes.

Queue identity, cursor, registrations, recent IDs, message ownership, and routes are bounded process-local state. Reconfiguration, unregister, agent unload, and shutdown advance generations and retire affected authority. By default queue loss starts from a fresh live tip and reports a possible gap.

Opt-in created-message catch-up registers the replacement live queue before bounded history retrieval. An identity-key-derived namespace stores only the native message high-water position with atomic replacement and exclusive process ownership. First use establishes a current baseline without historical replay. Later history and live creates merge by native message ID; offline mutations are not synthesized. The extension advances the highest completed observed prefix only after its own correlated canonical `message.delivered` fact returns through ordinary post-persistence subscription delivery. Filter changes do not rescan before the stored position.

The user approved these persistence, ordering, recovery, and downpath-acknowledgement semantics for clank ticket `1k2c`, satisfying [GATE-persistence-and-extension-interface-change-approval](../../../specs/GATE-persistence-and-extension-interface-change-approval.md).

The user approved exact Zulip inbound Markdown preservation, including a leading addressed bot mention, for ticket `0rpi`, satisfying [GATE-persistence-and-extension-interface-change-approval](../../../specs/GATE-persistence-and-extension-interface-change-approval.md).

Successful remote sends emit `message.sent_reported` before the terminal tool result. Ordinary mode retains bounded source reply and mutation ownership; send-only mode retains none. There is no remote/local transaction, durable outbox, automatic ambiguous retry, exactly-once guarantee, upload/download authority, or reconstruction of reply authority from replay.

Mandatory message reports and sole tool terminals use checked ordered output.
The event worker advances its queue cursor only through events whose required
report was published. Output failure retires the extension loop; progress and
notices remain best effort.
