---
name: tau-self-knowledge-source-code
description: Use when locating, inspecting, or setting up Tau source code, including debugging, detailed understanding, local checkout setup, upstream Radicle project details, clone commands, or the GitHub mirror.
advertise: false
---

# Tau source code

## Exact-byte inspection

Tool text escapes exact prompt-envelope closing sentinels; ordinary typed
results neutralize the `tau_internal` closing token. Literal and already-escaped
tokens can therefore look identical, without changing disk bytes.
For exact-byte reviews, inspect bounded hex (for example,
`sed -n '148,149p' crates/tau-proto/src/prompt_envelope.rs | od -An -tx1`),
or compare the bytes locally and print only a boolean rather than the source text.
See [SPEC-context-output-formatting](../../tau-harness/specs/SPEC-context-output-formatting.md#registered-injected-context-envelopes)
for the framing contract.

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

Reuse a checkout such as `~/.cache/tau/src`.
