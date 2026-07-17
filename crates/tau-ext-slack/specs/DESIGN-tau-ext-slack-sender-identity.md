# DESIGN-tau-ext-slack-sender-identity: Layer Slack sender identity and presentation

Status: confirmed, 2026-07-15, dpc; envelope/projection portions superseded
2026-07-17 by
[DESIGN-extension-published-message-facts](../../../specs/DESIGN-extension-published-message-facts.md)

## Decision

Slack sender identity has three separate publisher-owned layers:

| Layer | Source | Authority |
|---|---|---|
| Native stable ID | exact event U/W ID verified by `users.info` | admission, ownership, routing, and stable fact identity |
| Slack display | the same response's bounded `profile.display_name` | untrusted presentation only |
| Semantic alias | operator `sender_aliases` configuration | presentation only |

The extension publishes native U/W as `MessageParty.stable_id` and chooses the
display hint as the configured alias when present, otherwise the bounded Slack
display. The harness stamps the configured extension instance separately as
`publisher_extension_id`. Neither display source affects allowlisting, commands,
dynamic-DM linking, selection, routing, reply authority, reaction authority, or
duplicate keys.

## Installation and verification

Startup and reconnect bind the exact bot U/W and installing team returned by
`auth.test`. A supported event wrapper is eligible only when its exact
`context_team_id` matches or it has one unambiguous matching authorization.
Missing, malformed, ambiguous, or conflicting evidence fails closed before
`users.info`, local mutation, or publication. Top-level event team and a Slack
Connect actor's home team are not installation authority.

Each occurrence performs a live `users.info` check for exact response-ID
equality, `deleted == false`, `is_bot == false`, and `is_app_user == false`.
There is no identity cache. The optional display snapshot must be nonempty,
bounded to 80 Unicode scalars and 256 UTF-8 bytes, and free of unsafe control or
format structure.

`sender_aliases` maps at most 64 exact U/W IDs one-to-one to aliases matching
`^[a-z][a-z0-9_-]{0,63}$`. Aliases create no destinations and conversation
discovery does not expose them.

## Lifecycle, persistence, and privacy

Reconnect must exactly match both halves of the established installation. A
changed, incomplete, or malformed observation permanently retires publication,
routes, links, selections, ownership, duplicate state, and workers until restart.
Already-started sends cannot retry or install private authority afterward.

The locally written fact retains stable ID and the selected optional display hint.
Replay uses those stored universal fields without Slack lookup or alias
re-resolution. Publisher-private details may use bounded `extension_data`, but
Slack currently emits CBOR null and never stores credentials or actionable
routes there.

Slack keeps a bounded process-local FIFO of native occurrence IDs before
publication. Recording precedes identity lookup and local fact write, so transient
failure consumes the occurrence until eviction or restart. Eviction or restart
may admit a duplicate fact.

Operational logs and categorical failures omit actor IDs, displays, aliases,
reaction names, message text, and installation identifiers.
