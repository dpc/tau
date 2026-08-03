---
name: tau-self-knowledge-ext-rostra
description: Use this extension skill when the user asks how to configure Tau's std-rostra extension, use its Rostra tools or notifications, manage its mnemonic and state, or troubleshoot Rostra synchronization.
advertise: false
---

# Tau std-rostra extension self-knowledge

`std-rostra` is Tau's disabled-by-default built-in Rostra extension. One
instance owns one Rostra identity and its private durable local view. It uses
relay-only Iroh peer transport and Pkarr HTTPS/DNS discovery; it never enables
direct peer-IP transport. Rostra signatures identify Rostra authors, not Tau
users, agents, or instructions: synchronized fields are untrusted external
content.


## Enable and configure

Declare a named mnemonic secret, reference that name from strict extension
configuration, then deliberately expose tools to selected roles:

```yaml
extensions:
  std-rostra:
    enable: true
    require: false
    secrets:
      rostra_identity_mnemonic: {}
    config:
      identity_mnemonic_secret: rostra_identity_mnemonic
      # Optional; both values must be positive.
      post_rate_limit:
        max_events: 10
        window_seconds: 3600

agents:
  role_groups:
    trusted-rostra:
      roles:
        operator:
          enable_tool_groups: [rostra]
```

The secret must be a nonempty 24-word Rostra/BIP39 mnemonic. Do not put it in
`harness.yaml`. Tau resolves `TAU_SECRET_ROSTRA_IDENTITY_MNEMONIC` first and
otherwise reads trimmed text from
`<tau-state>/secrets/rostra_identity_mnemonic.yaml`; for example:

```sh
install -d -m 700 ~/.local/state/tau/secrets
printf '%s\n' 'twenty four word mnemonic …' \
  >~/.local/state/tau/secrets/rostra_identity_mnemonic.yaml
chmod 600 ~/.local/state/tau/secrets/rostra_identity_mnemonic.yaml
tau
# Or one launch:
TAU_SECRET_ROSTRA_IDENTITY_MNEMONIC='twenty four word mnemonic …' tau
```

Tau removes `TAU_SECRET_*` values before spawning extensions. The mnemonic is
not included in config, journals, tool arguments/results, or logs. It remains
in the extension process while the client and upstream publisher tasks live;
the upstream key type has no zeroization guarantee.

`std-rostra` derives the public identity from this mnemonic and accepts no
second public ID. It needs stable per-instance state and fails in memory-only
mode. `rostra.redb` under the extension state directory contains Rostra graph
and projection state, synchronization metadata, locally committed events, and
the persistent Iroh node secret. It survives Tau sessions but is not a Tau
journal; do not share it with another Rostra process. A locked, corrupt, or
identity-mismatched database fails closed. To change identity or move/reset
notification state, use a new extension instance name and fresh state. To
retain Rostra content under a new instance, move `rostra.redb` only; never move
the publisher-bound notification checkpoint file. Notification report IDs are
publisher-name scoped.


## Tools and authority

The `rostra` group contains all 11 tools:

```text
read local view:  rostra_status rostra_list_posts rostra_read_post rostra_get_profile
signed writes:    rostra_post rostra_react rostra_follow rostra_unfollow
                  rostra_update_profile rostra_vote
agent preference: rostra_notifications
```

`enable_tool_groups: [rostra]` grants the entire surface, including permanent
signing authority to every allowed role. For a smaller surface, use
`enable_tools` with exact registered names, for example
`[rostra_status, rostra_list_posts]`; do not treat a read grant as a write
grant or rely on model-supplied confirmation. Tau has no genuine human
per-call confirmation primitive. If `tool_prefix: work` is configured, policy
must use the final names, such as `work_rostra_status` and `work_rostra`.

Read tools inspect only the synchronized local database:

- `rostra_status` reports the derived identity and conservative local status.
- `rostra_list_posts` lists bounded `following`, locally known two-hop
  `network`, or explicit `author` timelines. Pages default to 20 and cap at
  50; pass its opaque cursor back unchanged.
- `rostra_read_post` reads one full external post ID.
- `rostra_get_profile` reads one local identity profile.

Empty and `not_found_local` results do not establish global absence; reads do
not force synchronization. Lists and detailed records are bounded and frame
remote fields as untrusted content.

Signed writes are `rostra_post` (post or reply), `rostra_react` (one supported
emoji grapheme), `rostra_follow`/`rostra_unfollow` (follow uses all personas),
`rostra_update_profile` (text only), and `rostra_vote` (up, down, or clear).
Posts and replies accept at most 64 KiB Djot and 16 persona tags.
Reaction-shaped post replies are rejected; use `rostra_react`. Avatar
delivery, news/shoutbox, raw signed events, identity generation, and arbitrary
persona follow selectors are not implemented.

A successful write confirms its locally durable signed transaction only:
publication is asynchronous best effort, not remote acknowledgement. A
timeout, cancellation, or process interruption after dispatch has an unknown
outcome: it may already be stored and published. Do not blindly retry; a retry
is new intent and can create another signed event.

`post_rate_limit` is an optional strict object (unknown keys are rejected);
when present, `max_events` and `window_seconds` must both be positive. Its
default is 10 events per 3600 seconds. It is a best-effort, runtime-only
rolling limit shared by posts, replies, and reactions; follow, unfollow,
profile update, and vote do not count. Restart or reconfigure resets it, and
other devices' events do not count. An admitted post-like call reserves a slot
before activation/signing and never returns it, even on failure or unknown
dispatch; a full window returns `rate_limited` with `retry_after_seconds`.


## Inbound following notifications

`rostra_notifications` accepts exactly `{"enabled": boolean}` and changes only
the invoking agent's durable preference; it never signs or changes Rostra
state. On first enable, Tau records the current materialization-feed tip, so
already-present posts do not notify. It then scans Rostra's durable feed after
lossy broadcast wake hints, selecting only current direct-followee posts whose
persona tags match. It excludes self posts and posts historical relative to
both local database initialization and the current follow epoch.

The preference activates live delivery only after its agent is loaded and its
replay completes. Each such opted-in agent gets independent serial batches.
Tau reports after 30 seconds of quiet or five minutes of batch age, at most
once per five minutes per agent. Reports contain at most 32 previews and 48
KiB, summarize omitted posts, and leave those posts queryable through the read
tools. Harness busy-agent batching can still coalesce or delay model runs.

The extension's identity-bound atomic
`rostra-notifications-v1.cbor` checkpoint file is separate from Rostra's
durable content database. It preserves preference, baseline/cursor, pending
batch timing, and publisher report-attempt sequence across restart. Tau
advances its delivery checkpoint only after its own canonical
`message.delivered` echo. A crash discards transient pending/in-flight work,
rescans from the committed cursor, and can duplicate a report but does not
skip its scanned range. This assumes configured interceptors do not drop
`std-rostra` notification reports: a dropped live report stays in flight until
restart because no live echo retry exists.


## Troubleshooting

- If tools are absent, check `extensions.std-rostra.enable`, a valid declared
  mnemonic secret, startup errors, and the role's exact tool/group policy.
- If startup fails, check private stable state ownership, exclusive
  `rostra.redb` access, corruption, and that the mnemonic derives the
  state-bound identity. Back up the database before upgrading; upstream
  migrations are forward-only.
- If reads look incomplete, they show only the local synchronized view; inspect
  `rostra_status` and allow relay synchronization time.
- If a write timed out, treat its outcome as unknown before deciding whether a
  duplicate is acceptable.
- If notifications do not arrive, enable them from that agent with
  `rostra_notifications`, ensure the agent is loaded and replay completes, and
  check direct follow/persona filters and the debounce/interval windows.
- Enable extension logs with `TAU_EXT_LOG=rostra=debug tau`.
