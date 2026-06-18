# tau-ext-xmpp

Disabled-by-default Tau extension for talking to agents over XMPP.

This MVP sends ordinary XMPP text over TLS only. It does **not** provide OMEMO or
other end-to-end encryption; the XMPP server/operator can read message content.

Prefer `routing.mode: muc` with a Prosody MUC service configured for private
Tau rooms so each registered Tau agent appears as a separate conversation.
`direct_resource` exists as a standards-minimal fallback.


## Prosody-oriented setup notes

For the recommended MUC mode, configure the MUC component so newly created Tau
rooms are private/hidden, persistent if desired, members-only for your allowed
human JIDs, and expose real JIDs to room occupants (`public_jids` in Prosody
terms). This MVP joins/creates rooms but does not submit room configuration
forms or member affiliation IQs, so privacy and membership policy must come from
server defaults or preconfiguration. Tau still fails closed on inbound MUC
messages without real-JID proof unless `trust_muc_membership: true` is
explicitly configured.


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
        service: conference.example.org
        room_prefix: tau
        expose_real_jids: true
        # Set true only if the room server enforces membership and intentionally
        # hides real JIDs from Tau.
        trust_muc_membership: false
```

Agents must call `xmpp_register(enabled: true)` before they can receive XMPP
prompts or use `xmpp_send`. Roles must opt into the tools because both tools are
registered with `enabled_by_default: false`. Outbound MUC messages are visible to
room occupants.

## Routing modes

- `muc` (recommended): creates/joins one room per Tau worker session and agent.
  This gives ordinary XMPP clients a separate conversation per registered agent,
  while Tau enforces `allowed_jids` from current real-JID presence when
  available.
- `direct_resource`: announces the current bound full JID to `default_recipient`
  and accepts only direct messages addressed exactly to that full JID. This mode
  supports only one registered agent per extension instance.
