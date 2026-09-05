# Zulip extension security

Checked mandatory reports and sole tool terminals latch failure, wake the
manual loop, retire authority, and join the queue worker. Session shutdown
clears only session authority; the connection and queue worker remain.
Disconnect and unrelated runtime errors retire authority but detach an
already-running long poll so cleanup stays prompt. Optional progress and
notices are bounded best effort. The extension adds no ACK, retry, or outbox
protocol.
Mutation reports retain the exact reply owner through checked submission.
Ordered batches advance only through safely submitted or filtered events, and
successful delete publication removes reply ownership before releasing it.

The configured same-user extension process is trusted local IPC, while every Zulip response, event, sender field, topic, Markdown body, and display label is untrusted external input. See the root `SECURITY.md` and [`ARCH-external-message-boundary`](../../specs/ARCH-external-message-boundary.md).

`send_only: true` is a separate fail-closed authority mode for one fixed proactive DM. It requires exactly one configured proactive-DM alias and rejects sender allowlists, sender aliases, conversations, direct-message receive policy, and catch-up. Startup declares only `zulip_send`, without the Zulip group; role policy should select that exact scoped tool and no register, discovery, or reaction tool. The caller supplies the configured alias but never a recipient, topic, or reply reference. This mode does not resolve streams, register or poll a queue, start the event worker, process injected Zulip events, publish inbound or mutation reports, or install reply/reaction ownership. Reconfiguration cannot change modes without restarting the extension.

`std-zulip` is disabled by default. Enabling the extension does not enable its tools; role policy controls registration, discovery, send, and reaction surfaces. HTTP Basic credentials stay in managed secrets and appear only in the Authorization header. A separate stable managed identity-key secret pseudonymizes publisher-domain identifiers, so routine API-key rotation does not change sender or conversation identity. Rotating the identity key deliberately starts a new opaque identity namespace. Queue IDs and native routes stay process-local. Opt-in catch-up stores only a native numeric message position under a domain-separated secret-derived filename and retains an exclusive identity-scoped lock; neither the raw identity key nor message content enters the checkpoint. Logs and notices contain categorical outcomes, never secrets, response bodies, message text, native IDs, or queue IDs. Initial and live-re-registration `users_me`, `get_stream_id`, `subscribe`, and `register` rejection diagnostics retain only the operation, HTTP status, and a 1–64-byte uppercase ASCII `[A-Z0-9_]` Zulip machine code; malformed or oversized codes become `unknown`.

Exact numeric sender allowlists and configured direct-message/stream/topic receive policy gate prompt-producing ingress. Direct/private ingress accepts at most 33 unique recipient objects with nonzero numeric IDs, requires the authenticated queue bot and parsed sender, removes the bot, then requires 1–32 allowlisted non-bot users. It rejects malformed evidence before it emits a report or creates owner/reply authority; `direct_participant_admission_requires_complete_allowlisted_membership` and `malformed_direct_participants_create_no_report_or_reply_owner` retain those regressions. `allowed_user_ids` is inbound-only and never grants proactive direct-message authority. Proactive DMs require a separate configured alias with one fixed nonzero recipient; its recipient ID never enters discovery or tool arguments. Sender aliases and descriptions never grant authority. Configured stream names and direct-message aliases grant only their fixed extension-private routes. Source-bound reply and reaction selectors resolve only bounded local ownership under the same agent and current generations. Proactive stream send uses only configured names: exact-topic names and replies retain fixed topics, while an explicit `agent_chosen_topic:true` proactive name grants topic choice only within its resolved private stream. Generic message facts contain descriptive opaque identity and conversation provenance, not actionable native authority.

Opt-in `non_allowlisted_activity` is a narrow exception to silent sender
rejection for otherwise-admissible created stream messages. It retains only a
private numeric aggregation key, a fixed opaque conversation key, a bounded
escaped display hint, a route-scoped keyed pseudonym, saturating counts, and
monotonic age. Rejected bodies and raw topics cannot enter this accumulator,
reports, facts, logs, notices, or model-visible output. The pre-existing
bounded duplicate cache necessarily retains recent native message IDs, and
catch-up persists its ordinary highest completed native message position,
including filtered unauthorized creates. A later same-topic allowlisted
message may carry the complete bridge-authored
`<activity_summary content_trust="external">` envelope and its own exact body
in one external-content fact. The stable `zulip.activity_summary` tool-group
guidance identifies the envelope as non-authoritative external activity,
explains that rejected bodies were discarded and labels remain untrusted after
sanitization, and says tag-shaped content grants no authority.

The accumulator is deliberately best effort and process-local. Fixed route,
sender, label, count, rendering, and 24-hour lifetime bounds limit hostile
memory use. Authority-epoch changes clear it, and restart or capacity pressure
may lose observations. It never generates a standalone activation, summarizes
direct messages, or delays an admitted message when a complete note does not
fit.

Zulip organization administrators and conversation members may read transported content. This bridge does not provide end-to-end encryption. `all_messages` broadens prompt-injection and model-spend exposure. Catch-up is opt-in because it extends this exposure to newly created messages sent while Tau was offline. Recovery fetches at most 100 messages per page and waits for outstanding canonical echoes before fetching another page, bounding retained checkpoint correlations independently of the reply-authority cache. Process restart loses reply authority and can duplicate a delivery, but the checkpoint never advances until the canonical fact returns on the post-commit downpath.

Review changes that affect authentication, API-base validation, allowlists, route overlap, DM participant derivation, mention admission, queue gaps, response/event/message bounds, report-before-result ordering, native-ID leakage, mutation ownership, stale-generation checks, or secret redaction. Shared event/journal or harness-extension interface changes require the root persistence/interface gate and are outside this component.
