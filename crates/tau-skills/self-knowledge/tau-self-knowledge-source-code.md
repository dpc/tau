---
name: tau-self-knowledge-source-code
description: >
  Use this skill when the user needs Tau source code for debugging, inspection,
  detailed understanding, local checkout setup, upstream Radicle project details,
  clone commands, or the GitHub mirror.
advertise: false
---

# Tau source code

For debugging or detailed understanding, inspect the Tau source code rather than guessing from behavior.

Primary upstream is Radicle:

- Project id: `rad:z3ToHcxKefTYxZEoCoDXmddUkK3a4`
- With the Radicle CLI: `rad clone rad:z3ToHcxKefTYxZEoCoDXmddUkK3a4`
- Public Radicle web/node example: <https://radicle.network/nodes/radicle.dpc.pw/rad:z3ToHcxKefTYxZEoCoDXmddUkK3a4>

There is also a GitHub mirror: <https://github.com/dpc/tau>.

## Optional extension projects

Tau's flake pins and re-exports the separately maintained service integrations,
but Tau does not bundle or enable their executables:

- Zulip: <https://radicle.network/nodes/radicle.dpc.pw/rad%3Az2LFTBWK7VpAwC3Bpxohkh91aqXd>
- Rostra: <https://radicle.network/nodes/radicle.dpc.pw/rad%3Az4LrrRivcgNjJii5wzbjTvA8ttt6o>
- Slack: <https://radicle.network/nodes/radicle.dpc.pw/rad%3Az3NJhEtKWCbHPa28wDQSYJ8eEfBjg>
- Telegram: <https://radicle.network/nodes/radicle.dpc.pw/rad%3Az3sPdSePnxtBvP9pTLwUwVgpMU68r>

Install the corresponding `tau-ext-*` package from the Tau flake, then enable
and configure its `std-*` instance explicitly. Telegram gateway-client mode also
uses the flake's `tau-telegram-gateway` package.

When an agent needs a local checkout, prefer a reusable cache location such as `~/.cache/tau/src` to avoid re-downloading the repository for every investigation.
