# tau-ext-xmpp security notes

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
- The default MUC room template includes a readable agent slug plus a compact
  40-bit, domain-separated BLAKE3 disambiguator derived from the full globally
  unique, validated agent id. `muc.room_template` is trusted operator policy and
  may omit the hash/randomness or use session/role/group identity; doing so accepts
  the resulting cross-process/restart collision and room-reuse risk.
  If two rendered room names ever collide in one process, registration fails
  closed instead of overwriting the existing room route.
- Text is treated as untrusted external input and is prefixed with XMPP
  message/channel/source context before being submitted to Tau. The
  model-visible prefix intentionally omits generated room labels, session ids,
  and agent ids.
- Tau sends unavailable presence for MUC rooms on unregister and session
  shutdown where the worker is still connected. After a successful MUC join,
  invite/fallback notices are best-effort, happen after `xmpp_register` success,
  and are cancelled by shutdown so bounded cleanup can prioritize unavailable
  presence; server history/occupant policy remains a deployment concern.
