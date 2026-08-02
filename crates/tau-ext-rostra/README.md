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
```

Supply `TAU_SECRET_ROSTRA_IDENTITY_MNEMONIC` when starting Tau. The harness
consumes and removes that environment variable before starting extensions; its
generic secret resolver otherwise reads
`<tau-state>/secrets/rostra_identity_mnemonic.yaml`. The extension starts
read-only; its first signed tool call lazily activates the identity. Activation
can publish a signed node announcement and starts best-effort background
publication and merge tasks.

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
instance or move the old directory.

Posts use bounded Djot and optional persona tags. Replies use `reply_to`;
reactions are single-emoji upstream social-post replies. Profile updates are
text-only and votes are `up`, `down`, or `clear`. Attachments, avatars, news,
shoutbox, raw event signing, identity creation, on-demand synchronization,
notifications, and inbound messages remain unsupported.

See [`ARCH-tau-ext-rostra`](specs/ARCH-tau-ext-rostra.md) and
[`SECURITY.md`](SECURITY.md) for the complete contract and trust boundary.
