# ARCH-tau-ext-xmpp: tau-ext-xmpp architecture

External ingress is constrained by [ARCH-external-message-boundary](../../../specs/ARCH-external-message-boundary.md).
Fail-closed admission and readiness behavior is specified by
[SPEC-tau-ext-xmpp-allowlist-and-default-recipient](SPEC-tau-ext-xmpp-allowlist-and-default-recipient.md)
and [SPEC-tau-ext-xmpp-readiness-waits](SPEC-tau-ext-xmpp-readiness-waits.md).
MUC join, authorization, rollback, and cleanup behavior is
[SPEC-tau-ext-xmpp-muc-lifecycle](SPEC-tau-ext-xmpp-muc-lifecycle.md).

`std-xmpp` is a disabled-by-default personal XMPP text bridge. It exposes only
`xmpp_register` and `xmpp_send`; the model never supplies arbitrary destination
JIDs. The extension does not connect to XMPP until an agent registers.
Per-instance generic `tool_prefix` values scope both tools and group `xmpp` for
multi-account deployments; semantic XMPP tags remain shared.
Configuration is applied only before the XMPP worker starts. Once an agent
registration starts the bridge, the worker owns its cloned `RuntimeConfig`,
including the resolved password and routing policy, and later `Configure`
messages fail with `ConfigError`. Restart Tau to apply changed XMPP credentials,
allowlists, routing, or message limits.

A single XMPP account may be used by multiple Tau processes concurrently because
this extension always requests a generated, high-entropy resource and then uses
the server-returned bound full JID.

The preferred routing mode is one MUC room per registered Tau agent id.
Registration waits up to 30 seconds for the initial XMPP online state,
joins the room, waits for exact `room/nick` self-presence, submits XEP-0045
instant-room owner config if status 201 reports a newly-created room, then
reports `xmpp_register` success. Only after that success response does Tau send
the XEP-0045 mediated invite plus direct fallback notice to the default
recipient; those notices are best-effort and must not delay registration success
or shutdown cleanup. Tau leaves the room with unavailable presence on unregister
or session shutdown.
Room identity is rendered by the strict Handlebars `muc.room_template`. Its
default, `{{agent_id}}-{{agent_hash}}`, preserves the full global agent id and a
domain-separated 40-bit BLAKE3 label over that id. Operators
may instead use agent, session, role, role-group, instance, or explicit random
inputs and may omit the hash entirely; the rendered value is the complete room
localpart and therefore defines the operator's cross-process/restart collision
policy. The extension caches replayed/live `agent.started` roles and reconstructed
`harness.roles_available` groups for render-time metadata. Before joining, the
worker rejects any normalized rendered room already active or pending for a
different agent instead of overwriting `room_to_agent`.
Once MUC join presence is sent, the worker records pending
non-routable room/nick state until setup succeeds so timeout,
configuration failure, dropped registration response, unregister, or shutdown
can still send unavailable leave presence. Direct full-resource chat is
available as a portability fallback and accepts inbound messages only when the
stanza `to` exactly matches the current bound full JID. If reconnect binds a
different resource,
direct-resource registrations are updated and the default recipient is notified
of the new full JID. Existing MUC history is not requested on join, and
delayed/history stanzas are dropped if a server sends them anyway. The MVP
submits only the empty instant-room owner form needed to unlock newly-created
rooms; it does not fetch full configuration forms or submit affiliation IQs.
Deployments must use Prosody/server defaults or preconfiguration for private,
hidden, and members-only room policy.

The worker is the source of truth for XMPP connection readiness: an authenticated
tokio-xmpp `Online` event sets the current bound full JID, and `Disconnected`
clears it plus connection-scoped MUC occupant real-JID cache. `xmpp_register`
waits up to 30 seconds for readiness after starting the bridge before issuing
registration. `xmpp_send` waits up to 30 seconds only after the bridge has
already been started; startup remains registration-driven, and if no registered
conversation exists after readiness the tool still fails with the explicit
`xmpp_register(enabled: true)` requirement.

Harness disconnect and extension drop request worker-wide shutdown. In-flight
command, reconnect, readiness, join, rejoin, stanza-send, and best-effort notice
work must be interrupted or bounded so the worker can prioritize unavailable MUC
leave presence under the remaining cleanup budget. Shutdown wakeups are
event-driven: async worker paths wait on notification instead of periodic
polling, while synchronous paths keep a cheap requested-state check.

After XMPP allowlist, room-membership, target, history, and own-message checks,
incoming text is emitted directly as `message.delivered_reported`. Accepted direct bare
JIDs or MUC real/occupant identities feed stateless opaque sender references; the
direct peer or room remains fact provenance but is not projected as a native
identifier into model context; it remains durable fact/UI provenance. Native
stanza IDs are hashed with sender/conversation identity, while missing IDs use a
process-unique local identity. The original stanza body is published without a
transport prefix according to
[DECISION-common-external-message-envelope](../../../specs/DECISION-common-external-message-envelope.md).

Successful `xmpp_send` calls emit `message.sent_reported` before their ordinary terminal
tool result. The sent report uses the original body and a bounded identity derived from
the unique tool call and locally authoritative conversation. Full-resource
routes, membership proof, and send authorization remain extension-local. Each
outbound stanza body is at most 4096 UTF-8 bytes; larger accepted tool messages
are split at character boundaries and carry visible part numbering so deployed
clients cannot silently discard the suffix. Multipart delivery is sequential
and non-atomic: a later failure leaves earlier parts visible, reports failed,
total, and completed part counts, and emits no `message.sent_reported`. All-part success
emits one `message.sent_reported` report containing the original unsplit tool text.

- The built-in extension is disabled by default and requires an explicit
  password secret plus a non-empty `allowed_jids` allowlist.
- Pre-start reconfiguration must not leave stale accepted config active after a
  `Configure` parse/validation error; a later registration must not start with
  old credentials, allowlists, routing, or message limits. Once the XMPP worker
  has started, it owns an immutable config snapshot: later `Configure` messages
  are reported as `ConfigError` diagnostics, and changing credentials,
  allowlists, or routing requires restarting Tau.
- The MVP is plaintext XMPP message content protected only by TLS. It does not
  implement OMEMO or any other end-to-end encryption, so the XMPP server and its
  operator can read messages.
- TLS certificate validation is always enabled; there is no insecure plaintext
  transport option.
- The model cannot choose arbitrary recipients. `xmpp_send` sends only to the
  registered agent's configured conversation. MUC sends are visible to room
  occupants. MUC registration sends a mediated invite and a direct fallback
  notice only to the configured, allowlisted `default_recipient`.
- Messages from JIDs outside `allowed_jids` are ignored before routing.
- MUC mode requires real-JID visibility by default. If real JIDs are hidden, the
  extension fails closed unless `trust_muc_membership: true` is explicitly set.
  Tau submits instant-room configuration only to unlock newly-created rooms; it
  does not currently relax privacy settings or grant member affiliations.
  Deployments must enforce privacy and any members-only policy at the server or
  room-default layer.
- The default MUC room template includes the full global durable agent id plus a
  compact 40-bit, domain-separated BLAKE3 disambiguator derived from that id.
  `muc.room_template` is trusted operator policy and
  may omit the hash/randomness or use session/role/group identity; doing so accepts
  the resulting cross-process/restart collision and room-reuse risk.
  If two rendered room names ever collide in one process, registration fails
  closed instead of overwriting the existing room route.
- Text is treated as untrusted external input and published as the original body
  in a transport-neutral `message.delivered_reported` report. Sender/conversation metadata
  describes provenance without exposing actionable routes, membership proof,
  Tau session IDs, or Tau agent IDs.
- Tau sends unavailable presence for MUC rooms on unregister and session
  shutdown where the worker is still connected. After a successful MUC join,
  invite/fallback notices are best-effort, happen after `xmpp_register` success,
  and are cancelled by shutdown so bounded cleanup can prioritize unavailable
  presence; server history/occupant policy remains a deployment concern.
