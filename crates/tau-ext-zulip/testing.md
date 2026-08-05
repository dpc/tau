# Zulip testing

Tests are hermetic. Loopback fake Zulip servers assert queue registration and
event-poll method/path, HTTP Basic headers, raw-Markdown registration, a
257-event bounded response, and credential nonexposure. They also cover
content-free startup rejection diagnostics for `users_me` and `register`,
including HTTP status, bounded Zulip machine codes, and malformed response
handling. No live organization, credential, DNS, webhook, or wall-clock sleep
is required.

Injected-client tests cover strict unknown-field and allowlist validation,
official outer-event mention flags, one exact stream/topic and DM route,
participant canonicalization, duplicate/self suppression, source-bound and
proactive sends, explicit agent-chosen-topic authorization including the empty
general-chat topic, incoming edit/reaction/delete reports, stale ingress
generation, delayed-enable/later-unregister ordering, report-before-result
ordering, and successful queue-expiry
replacement with a content-free gap notice. Harness tests remain responsible
for generic report canonicalization, persistence, replay, projection, and wake
semantics. Live Zulip compatibility, failed queue-registration backoff, every
configuration rejection, and other provider-specific error variants are not claimed
by the hermetic suite.

Checkpoint tests cover the default-disabled flag, first-use baseline,
sender/route filtering, post-commit echo advancement, atomic replacement,
checkpoint-write retry, report-submission barriers, unregister lifecycle,
corruption rejection, namespace secrecy, and exclusive identity ownership.
The fake history API and loopback request-shape test keep page sizes and Zulip
anchor parameters deterministic.
