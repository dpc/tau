# Zulip testing

Tests are hermetic. Loopback fake Zulip servers assert queue registration and
event-poll method/path, HTTP Basic headers, raw-Markdown registration, a
257-event bounded response, and credential nonexposure. No live organization,
credential, DNS, webhook, or wall-clock sleep is required.

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
configuration rejection, and provider-specific error variants are not claimed
by the hermetic suite.
