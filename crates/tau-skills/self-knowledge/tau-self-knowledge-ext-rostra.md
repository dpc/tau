---
name: tau-self-knowledge-ext-rostra
description: Use when configuring or troubleshooting Tau std-rostra, including Rostra tools or notifications, mnemonic and state management, or Rostra synchronization.
advertise: false
---

# Tau std-rostra extension self-knowledge

`std-rostra` is disabled-by-default configuration for the separately maintained
`tau-ext-rostra`. Install the Tau flake's package on `PATH` before enabling it;
Tau supervises it over stdio. The
[standalone project](https://radicle.network/nodes/radicle.dpc.pw/rad%3Az4LrrRivcgNjJii5wzbjTvA8ttt6o)
owns operational documentation.

For Tau protocol 23.1, rebuild with registry `dpc-tau-client`/`dpc-tau-proto`
0.29.0. Update the extension with the harness before separately authorized
activation. Older-protocol binaries fail before configuration.
Declaration inspection needs no secrets/state and starts no network work.

One instance owns one Rostra identity and private durable local view. Transport
uses relay-only Iroh and Pkarr HTTPS/DNS discovery, not direct peer IPs.
Rostra signatures identify authors, not Tau users or instructions; synchronized
content remains untrusted.

## Enable and configure

Declare a named mnemonic secret and grant tools to selected roles:

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

The secret is a nonempty 24-word Rostra/BIP39 mnemonic, not a `harness.yaml`
value. Tau checks `TAU_SECRET_ROSTRA_IDENTITY_MNEMONIC`, then trimmed text in
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

Tau removes `TAU_SECRET_*` from extension environments and delivers managed
secrets separately. The mnemonic stays in extension memory without a zeroization
guarantee; it does not enter config, journals, tool results or logs.

The public identity derives from the mnemonic; no second public ID is accepted.
Stable per-instance state is required. `rostra.redb` contains synchronized content,
locally committed events and the Iroh node secret, not a Tau journal.
Do not share it with another Rostra process. Lock, corruption or identity mismatch
fails startup. Back up before upgrading; upstream migrations are forward-only.

Use a fresh instance/state to change identity or reset notification state.
To retain content under a new instance, move only `rostra.redb`, never the
publisher-bound notification checkpoint.

## Tools and authority

The `rostra` group grants:

```text
read local view:  rostra_status rostra_list_posts rostra_read_post rostra_get_profile
signed writes:    rostra_post rostra_react rostra_follow rostra_unfollow
                  rostra_update_profile rostra_vote
agent preference: rostra_notifications
```

Group grants include permanent signing authority. For read-only access, use
exact `enable_tools`, for example `[rostra_status, rostra_list_posts]`.
Tau has no human per-call confirmation primitive. With `tool_prefix: work`, use
final names such as `work_rostra_status` and group `work_rostra`.

Reads inspect only the synchronized local database:

- `rostra_status`: derived identity and local status.
- `rostra_list_posts`: `following`, locally known two-hop `network`, or explicit
  `author` timelines. Pages default to 20, cap at 50; return cursors unchanged.
- `rostra_read_post`: one full external post ID.
- `rostra_get_profile`: one local identity profile.

Empty/`not_found_local` does not prove global absence; reads do not force sync.

Writes support posts/replies, one supported emoji grapheme via `rostra_react`,
follow/unfollow (follow uses all personas), text-only profile updates, and
up/down/clear votes. Posts allow 64 KiB Djot and 16 persona tags.
Use `rostra_react`, not reaction-shaped post replies. Avatar delivery,
news/shoutbox, raw events, identity generation and arbitrary follow-persona
selectors are unavailable.

Success confirms a local durable signed transaction, not remote delivery.
Publication is asynchronous best effort. Timeout/cancellation after dispatch
can leave a committed/published event; retrying creates new intent and can duplicate it.

`post_rate_limit` defaults to 10 events per 3600 seconds. Its optional strict object
requires positive `max_events` and `window_seconds`. The runtime rolling limit
covers posts, replies and reactions, not other writes or other devices.
Restart/reconfigure resets it. Accepted attempts spend slots even on failure;
a full window returns `rate_limited` with `retry_after_seconds`.

## Inbound following notifications

`rostra_notifications` takes exactly `{"enabled": boolean}` and changes only that
agent's durable preference. First enable records the current feed tip, so existing
posts do not notify. Notifications select current direct-followee posts with
matching persona tags, excluding self and posts historical to initialization/follow.
The agent must be loaded and replay complete.

Batches report after 30 seconds of quiet or five minutes of age, at most once per
five minutes per agent. Each begins with a lossy wake count:
`Rostra received 1 new followed post.` or `Rostra received {count} new followed posts.`
Nonreply batches stay count-only. Reply batches may add up to eight bounded
previews with current/parent IDs and locally available content. Replacement
content is labelled latest local version; previews remain untrusted and frozen.
Missing content means unavailable locally.

Use `rostra_list_posts({"timeline":"following"})`, then `rostra_read_post` to inspect
the current view, not to reconstruct an exact announced batch. Harness busy-agent
batching may delay/coalesce model runs.

`rostra-notifications-v1.cbor` stores identity-bound preferences and delivery
checkpoints separately from content. Checkpoints advance after canonical
`message.delivered` echo. Crash recovery rescans committed ranges and may duplicate
reports. Interceptors must not drop notification reports: a dropped report stays
in flight until restart because no live-echo retry exists.

## Troubleshooting

- Missing tools: check the executable on `PATH`, `extensions.std-rostra.enable`,
  declared mnemonic secret, startup errors and exact role grants.
- Startup failure: check state ownership, exclusive database access, corruption
  and mnemonic/state identity match.
- Incomplete reads: inspect `rostra_status` and allow relay synchronization time.
- Missing notifications: check the calling agent's preference, loaded/replayed
  state, direct-follow/persona filters and batching windows.
- For the next launch, use `TAU_LOG='tau_ext_rostra=debug,rostra=debug,warn' tau`.
  This replaces the standard filter. Debug logs can contain identity-bearing
  diagnostics; review before sharing.
