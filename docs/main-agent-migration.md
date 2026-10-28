# Main-agent and protocol 16 migration

## Harness configuration

Remove `agents.default_role` and `inter_session.receiver`, including the receiver's
`role` and `auto_start` fields. Tau rejects these as unknown fields, including in
profiles. Assign role-only priorities instead; lower numbers win:

```yaml
agents:
  role_groups:
    coordination:
      roles:
        coordinator:
          main_priority: 100
    bots:
      roles:
        project-bot:
          main_priority: 10
inter_session:
  review_direct_messages: true
```

`main_priority` is an optional nonnegative integer. After merging overlays and
filtering disabled roles, the lowest number wins. Zero is valid; an absent or null
effective value means no candidate. Ties at the minimum fail configuration; every nonwinning
duplicate priority group produces a startup warning. An unavailable winning role
fails explicitly rather than falling back. Ordinary workers can still use that
role. Tau creates main on demand without a prompt and binds its identity to the
session; it does not select an idle worker. Existing mainless sessions still
resume. Explicit `--role` and role/group order
select ordinary work; main designation does not replace those controls.

The equivalent attributes in a Nix-generated harness configuration are:

```nix
{
  agents.role_groups.coordination.roles.coordinator.main_priority = 100;
  agents.role_groups.bots.roles.project-bot.main_priority = 10;
  inter_session.review_direct_messages = true;
}
```

Delete the old attributes from the generating module as well as any generated
YAML. Do not retain an empty `receiver` object or set retired keys to null.
Bot-account overlays should give their main role priority 10; the ordinary
coordinator uses 100. This lets bot overlays win without clearing the coordinator.
Later overlays replace the same role's number rather than adding to it or taking
the minimum.
Explicit null clears a role's priority where the configuration format supports
null; omitting the field from an overlay keeps its earlier value. Tau rejects the
unshipped boolean role field `main`.
New priorities apply only to an unbound session; they never replace a previously
bound canonical main on restart.

Remote exact-worker messages now default to main review. Set
`inter_session.review_direct_messages = false;` to retain direct delivery.
Bare-session and exact-main input goes to canonical main. Mainless sessions with
review enabled reject remote exact-worker input; their journals remain valid.
External adapter receiver registration, permissions, watches, human CLI exact
messages, and local exact messages are separate and unchanged.

## SDK consumers

The shared protocol is **16.0**. Both registry packages are published and have
passed registry-only consumer acceptance on Rust 1.91:

```toml
tau-proto = { package = "dpc-tau-proto", version = "=0.16.0" }
tau-client = { package = "dpc-tau-client", version = "=0.16.0" }
```

Update external consumers' manifest and lockfile pins and any Nix package source
and dependency hashes. Rebuild every deployed extension/client using the shared
SDK, including those in separately maintained bridge repositories. This document
is a handoff; SDK publication does not modify external repositories or personal
configuration, deploy, or restart live sessions. See the
[SDK release acceptance record](sdk-releases.md#protocol-160-release-acceptance)
for checksums and source-identifying tags. New readers accept older mainless
history; old readers cannot decode the new variants. Journal framing is unchanged.

Consumers must accept the new main membership/replay fields and transient
unavailable-runtime notification. Peer senders must preserve the reviewed
disposition: the ACK recipient is main, with `review_recipient` identifying the
original intended worker. Do not translate it to worker-delivered success.
Tau rejects reviewed input from peers that cannot express this disposition.

The model-facing `message_release` tool accepts only a retained source reference.
Only the current canonical main may call it. A retained linked worker receipt
makes retries report already delivered, including after restart. If the original
worker history is unloaded, load that worker before retrying; release does not
revive it or open another session's leased journal. This is not a global
exactly-once guarantee and does not promise worker inference or a reply.
