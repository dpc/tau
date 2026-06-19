# tau-ext-xmpp security notes

- The built-in extension is disabled by default and requires an explicit
  password secret plus a non-empty `allowed_jids` allowlist.
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
- MUC room names are routing keys derived from the full Tau session id and full
  validated agent id through lowercase hex encodings. Distinct agent ids must
  not collapse after XMPP JID normalization, because that would risk cross-agent
  prompt delivery.
- Text is treated as untrusted external input and is prefixed with XMPP source
  context before being submitted to Tau.
- Tau sends unavailable presence for MUC rooms on unregister and session
  shutdown where the worker is still connected. After a successful MUC join,
  failures in invite/fallback notice delivery must still leave tracked state that
  can be cleaned up; server history/occupant policy remains a deployment
  concern.
