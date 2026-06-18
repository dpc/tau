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
  occupants.
- Messages from JIDs outside `allowed_jids` are ignored before routing.
- MUC mode requires real-JID visibility by default. If real JIDs are hidden, the
  extension fails closed unless `trust_muc_membership: true` is explicitly set.
  Tau does not currently configure MUC privacy or member affiliations itself;
  deployments must enforce that at the server or room-default layer.
- Text is treated as untrusted external input and is prefixed with XMPP source
  context before being submitted to Tau.
