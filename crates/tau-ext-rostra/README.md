# tau-ext-rostra

`tau-ext-rostra` provides Tau's disabled-by-default `std-rostra` integration.
It runs one full Rostra client with relay-only Iroh peer transport and Pkarr
HTTPS/DNS discovery; it never enables direct peer-IP transport. It reads a
Tau-managed 24-word identity mnemonic and derives the public identity rather
than accepting a duplicate public ID.

```yaml
extensions:
  std-rostra:
    enable: true
    require: false
    secrets:
      rostra_identity_mnemonic: {}
    config:
      identity_mnemonic_secret: rostra_identity_mnemonic
      post_rate_limit:
        max_events: 10
        window_seconds: 3600
```

Supply `TAU_SECRET_ROSTRA_IDENTITY_MNEMONIC` when starting Tau. The harness
consumes and removes that environment variable before starting extensions; its
generic secret resolver otherwise reads
`<tau-state>/secrets/rostra_identity_mnemonic.yaml`. The extension starts
read-only; its first signed tool call lazily activates the identity. Activation
can publish a signed node announcement and starts best-effort background
publication and merge tasks.

`post_rate_limit` is optional and strict. It defaults to ten shared
post-like attempts per rolling hour; both `max_events` and `window_seconds`
must be positive. Posts, replies, and emoji reactions share this runtime-only
guard; follows, unfollows, profile updates, and votes do not consume it. The
serialized write lane reserves an attempt before signing and does not roll it
back after dispatch. A full window returns `rate_limited` with
`retry_after_seconds`. Restarting or reconfiguring the extension resets this
best-effort guard; synchronized events from other devices do not count.

The read tools are `rostra_status`, `rostra_list_posts`, `rostra_read_post`,
and `rostra_get_profile`. The authenticated write tools are `rostra_post`,
`rostra_react`, `rostra_follow`, `rostra_unfollow`,
`rostra_update_profile`, and `rostra_vote`. A successful write means only that
the signed event reached the local database. Peer and Pkarr publication are
asynchronous best effort; cancellation, timeout, or process death can leave a
possibly completed write, and retrying can create another event.

`std-rostra` owns `<state_dir>/rostra.redb` exclusively. The database survives
Tau sessions and contains the public graph, signed local events, and Rostra's
persistent Iroh node secret; the extension never writes the mnemonic into this
database. The generic Tau secret resolver can instead store a supplied
file-backed secret as described above. Memory-only mode fails startup. Changing
the derived identity for existing state fails closed; use another extension
instance or move the old directory with a new instance name so publisher-scoped
notification IDs cannot repeat.

Posts use bounded Djot and optional persona tags. Replies use `reply_to`;
reactions are single-emoji upstream social-post replies. Profile updates are
text-only and votes are `up`, `down`, or `clear`. Attachments, avatars, news,
shoutbox, raw event signing, identity creation, on-demand synchronization, and
arbitrary inbound messages remain unsupported.

`rostra_notifications` is an agent-scoped preference with exactly
`{"enabled": true|false}`. Enabling records the current local social-post
materialization tip, so it never announces older feed rows. It reports only
direct followees whose persona selector matches, and excludes self-authored
posts. Rostra's lossy post broadcast is only a wake hint; the extension
reconciles the bounded durable materialization feed. A report becomes eligible
no sooner than 30 seconds after the last eligible row and at five minutes after
the first; canonical delivery can be delayed by normal harness processing. It
never emits more than once per agent every five minutes. That rate limit applies to Rostra
reports and their normal harness wakes, not model runs: the harness may batch,
coalesce, or delay work for a busy agent. Each report previews at most 32 posts
and 48 KiB, summarizes omitted posts, and leaves every post queryable through
the existing pull tools.

See [`ARCH-tau-ext-rostra`](specs/ARCH-tau-ext-rostra.md) and
[`SECURITY.md`](SECURITY.md) for the complete contract and trust boundary.
