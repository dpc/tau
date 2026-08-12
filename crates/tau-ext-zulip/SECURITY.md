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

`std-zulip` is disabled by default. Enabling the extension does not enable its tools; role policy controls registration, discovery, send, and reaction surfaces. HTTP Basic credentials stay in managed secrets and appear only in the Authorization header. A separate stable managed identity-key secret pseudonymizes publisher-domain identifiers, so routine API-key rotation does not change sender or conversation identity. Rotating the identity key deliberately starts a new opaque identity namespace. Queue IDs and native routes stay process-local. Opt-in catch-up stores only a native numeric message position under a domain-separated secret-derived filename and retains an exclusive identity-scoped lock; neither the raw identity key nor message content enters the checkpoint. Logs and notices contain categorical outcomes, never secrets, response bodies, message text, native IDs, or queue IDs. Initial and live-re-registration `users_me`, `get_stream_id`, `subscribe`, and `register` rejection diagnostics retain only the operation, HTTP status, and a 1–64-byte uppercase ASCII `[A-Z0-9_]` Zulip machine code; malformed or oversized codes become `unknown`.

Exact numeric sender allowlists and configured direct-message/stream/topic receive policy gate prompt-producing ingress. `allowed_user_ids` is inbound-only and never grants proactive direct-message authority. Proactive DMs require a separate configured alias with one fixed nonzero recipient; its recipient ID never enters discovery or tool arguments. Sender aliases and descriptions never grant authority. Configured stream names and direct-message aliases grant only their fixed extension-private routes. Source-bound reply and reaction selectors resolve only bounded local ownership under the same agent and current generations. Proactive stream send uses only configured names: exact-topic names and replies retain fixed topics, while an explicit `agent_chosen_topic:true` proactive name grants topic choice only within its resolved private stream. Generic message facts contain descriptive opaque identity and conversation provenance, not actionable native authority.

Zulip organization administrators and conversation members may read transported content. This bridge does not provide end-to-end encryption. `all_messages` broadens prompt-injection and model-spend exposure. Catch-up is opt-in because it extends this exposure to newly created messages sent while Tau was offline. Recovery fetches at most 100 messages per page and waits for outstanding canonical echoes before fetching another page, bounding retained checkpoint correlations independently of the reply-authority cache. Process restart loses reply authority and can duplicate a delivery, but the checkpoint never advances until the canonical fact returns on the post-commit downpath.

Review changes that affect authentication, API-base validation, allowlists, route overlap, DM participant derivation, mention admission, queue gaps, response/event/message bounds, report-before-result ordering, native-ID leakage, mutation ownership, stale-generation checks, or secret redaction. Shared event/journal or harness-extension interface changes require the root persistence/interface gate and are outside this component.
