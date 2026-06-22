# tau-ext-xmpp

Disabled-by-default Tau extension for talking to agents over XMPP.

This MVP sends ordinary XMPP text over TLS only. It does **not** provide OMEMO or
other end-to-end encryption; the XMPP server/operator can read message content.

Prefer `routing.mode: muc` with a Prosody MUC service configured for private
Tau rooms so each registered Tau agent appears as a separate conversation.
`direct_resource` exists as a standards-minimal fallback.


## Prosody-oriented setup notes

For the recommended MUC mode, configure the MUC component so newly created Tau
rooms are private/hidden, persistent if desired, and expose real JIDs to room
occupants (`public_jids` in Prosody terms). Tau waits for its MUC self-presence
and, when the server reports a newly-created room, submits the XEP-0045
instant-room owner form to unlock the room before reporting `xmpp_register`
success. Tau does not yet grant member affiliations; if the server default is
members-only, invited users still need preconfigured/server-side membership.
Tau fails closed on inbound MUC messages without real-JID proof unless
`trust_muc_membership: true` is explicitly configured.

Representative Prosody/NixOS sketch:

```nix
services.prosody = {
  enable = true;
  modules = {
    disco = true;
    roster = true;
    saslauth = true;
    tls = true;
    ping = true;
    smacks = true;
    mam = true;      # useful for humans; Tau does not query history in the MVP
    carbons = true;  # server support is fine; Tau does not enable carbons
  };

  virtualHosts."example.org" = {
    enabled = true;
    domain = "example.org";
    extraConfig = ''
      authentication = "internal_hashed"
      c2s_require_encryption = true
    '';
  };

  muc = [{
    domain = "conference.example.org";
    restrictRoomCreation = "local";
    extraConfig = ''
      modules_enabled = { "muc_mam"; }
      muc_room_default_public = false
      muc_room_default_persistent = true
      muc_room_default_members_only = false
      muc_room_default_moderated = false
      muc_room_default_public_jids = true
      muc_log_by_default = true
      muc_log_expires_after = "4w"
    '';
  }];
};
```

Ensure DNS/SRV records and certificates cover the account domain and MUC
subdomain. If room creation is restricted, the Tau XMPP account must be allowed
to create rooms. If your MUC service hides real JIDs, keep
`trust_muc_membership: false` unless the room is genuinely members-only and the
server-side membership list is the intended authorization boundary.

## Example configuration

```yaml
extensions:
  std-xmpp:
    enable: true
    secrets:
      xmpp_password: {}
    config:
      jid: tau@example.org
      password_secret: xmpp_password
      allowed_jids: [me@example.org]
      default_recipient: me@example.org
      routing: { mode: muc }
      muc:
        # Domain-only MUC service JID; localparts/resources are rejected.
        service: conference.example.org
        room_prefix: tau
        expose_real_jids: true
        # Set true only if the room server enforces membership and intentionally
        # hides real JIDs from Tau.
        trust_muc_membership: false
        # Send a formal XEP-0045 mediated invite plus a direct fallback notice.
        invite_default_recipient: true
      # Default: 16384 bytes. Valid range: 1..=131072 (128 KiB).
      max_message_bytes: 16384
```

Agents must call `xmpp_register(enabled: true)` before they can receive XMPP
prompts or use `xmpp_send`. Roles must opt into the tools because both tools are
registered with `enabled_by_default: false`. Outbound MUC messages are visible to
room occupants. Registration and sends wait up to 30 seconds for the XMPP stream
to become online/authenticated before returning a readiness error; `xmpp_send`
still requires an existing registered conversation after that wait.

## Routing modes

- `muc` (recommended): creates/joins one room per Tau session id and agent id.
  This gives ordinary XMPP clients a separate conversation per registered agent,
  while resumed Tau sessions return to the same room address. The room localpart
  is a short readable label like `tau-duvp2c-manager-m4tptqqs`:
  `<room_prefix>-<session-slug>-<agent-slug>-<8-char-disambiguator>`. Slugs are
  normalized lowercase hints capped to short lengths; generated-looking agent
  suffixes such as `-Y3KG` are omitted from the visible slug. The disambiguator
  is compact base32 over a domain-separated BLAKE3 label of the full Tau session
  id plus full agent id, so distinct sessions/agents remain collision-resistant
  after XMPP JID normalization without exposing long raw ids. Tau sends a formal
  XEP-0045 mediated invite to `default_recipient` plus a direct fallback notice
  with the room JID, and enforces `allowed_jids` from current real-JID presence
  when available. Registration waits for the exact post-join `room/nick`
  self-presence or presence error. A self-presence with status 201 triggers an
  instant-room owner config submit; presence/config errors or timeouts are
  returned from `xmpp_register` instead of silently claiming a usable room.
  `xmpp_register` success is returned after the room is joined and unlocked but
  before best-effort invite/fallback notice delivery; slow or failed notices do
  not roll back a usable room and are cancelled during shutdown. Tau still tracks
  and leaves the room on registration rollback, unregister, or shutdown.
  Changing the room-name derivation means existing rooms
  created by older Tau builds are not reused; users may leave or delete old
  `tau-s...-a...` rooms manually.
- `direct_resource`: announces the current bound full JID to `default_recipient`
  and accepts only direct messages addressed exactly to the current server-bound
  full JID. This mode supports only one registered agent per extension instance;
  use `routing.mode: muc` for multiple Tau agents or separate conversations. If
  reconnect changes the resource, Tau sends the default recipient a new address
  notice.

Tau requests zero MUC history on join and drops delayed/history message stanzas
if they are still delivered, so initial room backlog is not converted into
prompts. Tau sends unavailable presence when unregistering an agent or shutting
down a session so the XMPP account leaves no-longer-registered MUC rooms.

## Troubleshooting Conversations/Android MUC replies

In MUC mode, reply in the joined room conversation, not in the 1:1 direct notice
chat from the Tau XMPP account. Direct notice replies are ignored because they do
not identify which registered agent room should receive the prompt. If room
messages still do not reach Tau, check Tau logs for "dropping muc message without
real jid proof"; that means the MUC service is hiding occupant real JIDs and Tau
is correctly failing closed unless `trust_muc_membership: true` is explicitly
configured for a members-only server-side room.
