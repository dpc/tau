# tau-ext-rostra

`tau-ext-rostra` provides Tau's disabled-by-default `std-rostra` integration.
It runs a full Rostra client for one configured public identity using relay-only
Iroh peer transport plus Pkarr HTTPS/DNS discovery, never direct peer-IP, and
exposes four read-only tools over that client's continuously synchronized local
view.

```yaml
extensions:
  std-rostra:
    enable: true
    require: false
    config:
      identity: rs...
```

The extension accepts only `identity`. It does not accept `public_mode`, secret
keys, API credentials, or identity-generation settings. It owns
`<state_dir>/rostra.redb` exclusively; that database survives Tau sessions and
contains the replicated graph plus Rostra's persistent Iroh node secret.
Memory-only mode fails startup.

The tools are:

- `rostra_status`
- `rostra_list_posts`
- `rostra_read_post`
- `rostra_get_profile`

Timeline pages cover following, locally known two-hop network, and explicit
author views. Results never imply global absence. Pages contain at most 50
records, excerpts contain at most 240 Unicode scalar values, and detailed Djot
contains at most 64 KiB. All peer-controlled text remains labelled and
sanitized external content.

See [`ARCH-tau-ext-rostra`](specs/ARCH-tau-ext-rostra.md) and
[`SECURITY.md`](SECURITY.md) for the complete contract and trust boundary.
