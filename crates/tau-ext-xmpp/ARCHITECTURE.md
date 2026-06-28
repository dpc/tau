# tau-ext-xmpp architecture

`std-xmpp` is a disabled-by-default personal XMPP text bridge. It exposes only
`xmpp_register` and `xmpp_send`; the model never supplies arbitrary destination
JIDs. The extension does not connect to XMPP until an agent registers.
Configuration is applied only before the XMPP worker starts. Once an agent
registration starts the bridge, the worker owns its cloned `RuntimeConfig`,
including the resolved password and routing policy, and later `Configure`
messages fail with `ConfigError`. Restart Tau to apply changed XMPP credentials,
allowlists, routing, or message limits.

A single XMPP account may be used by multiple Tau processes concurrently because
this extension always requests a generated, high-entropy resource and then uses
the server-returned bound full JID.

The preferred routing mode is one MUC room per registered Tau session id and
agent id. Registration waits up to 30 seconds for the initial XMPP online state,
joins the room, waits for exact `room/nick` self-presence, submits XEP-0045
instant-room owner config if status 201 reports a newly-created room, then
reports `xmpp_register` success. Only after that success response does Tau send
the XEP-0045 mediated invite plus direct fallback notice to the default
recipient; those notices are best-effort and must not delay registration success
or shutdown cleanup. Tau leaves the room with unavailable presence on unregister
or session shutdown.
Room identity must remain stable and collision-resistant for registered routing
keys after XMPP JID normalization: the room localpart is
`<room_prefix>-<session-slug>-<agent-slug>-<8-char-disambiguator>`. The slugs are
short, lowercase, localpart-safe hints derived from the Tau session id and agent
id; generated-looking agent suffixes such as `-Y3KG` are omitted from the slug.
The disambiguator is a compact base32 encoding of a domain-separated BLAKE3 label
over the full Tau session id and full validated `AgentId`, so truncated/readable
slugs are not routing authority. The short disambiguator is collision-resistant,
not injective; before joining, the worker rejects any normalized generated room
already active or pending for a different agent instead of overwriting
`room_to_agent`. Once MUC join presence is sent, the worker records pending
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
leave presence under the remaining cleanup budget.

Incoming XMPP text is emitted as `extension.prompt_submit_request`. The harness
validates the target loaded agent and owns the durable prompt fact. The
model-visible text uses a compact transport/channel prefix that must not expose
generated room labels, room JIDs, Tau session ids, or Tau agent ids:

- `[xmpp room message from <bare-jid>]: <body>` for MUC messages with verified
  real-JID proof.
- `[xmpp room message from occupant <sanitized-nick>]: <body>` for explicitly
  trusted hidden-real-JID MUC membership.
- `[xmpp room message]: <body>` when no source label is shown.
- `[xmpp direct message from <bare-jid>]: <body>` for direct-resource fallback.

Occupant labels are only weak room-local display hints and are sanitized so they
cannot close or spoof the bracketed prefix.
