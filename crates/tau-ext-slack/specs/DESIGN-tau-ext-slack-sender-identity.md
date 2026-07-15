# DESIGN-tau-ext-slack-sender-identity: Layer Slack sender identity and presentation

Status: confirmed, 2026-07-15, dpc

**Beads:** `tau-agent-qv6o`, `tau-agent-damy`
**Related:** [DESIGN-tau-ext-slack-sender-admission](DESIGN-tau-ext-slack-sender-admission.md),
[DESIGN-tau-ext-slack-safe-source-mentions](DESIGN-tau-ext-slack-safe-source-mentions.md),
and [DESIGN-canonical-transport-ingress](../../../specs/DESIGN-canonical-transport-ingress.md)

## Decision

Slack sender identity has four deliberately separate layers:

| Layer | Source | Authority |
|---|---|---|
| Native stable ID | exact event U/W ID verified by `users.info` | admission, ownership, routing, and model-primary identity |
| Slack display | same successful `users.info` response's `profile.display_name` | untrusted UI presentation only |
| Semantic alias | operator `sender_aliases` configuration | presentation only, explicitly marked `operator_configured` |
| Source scope | harness-stamped transport instance | scopes the configured alias to one local transport instance |

Display and alias never change allowlist status, commands, dynamic-DM linking,
selection, routing, ownership, reply authority, reaction authority, or
deduplication authority. `sender_allowlisted` remains independent.

The provider envelope keeps the native stable ID as `sender` and may add
`sender_alias`, `sender_alias_authority="operator_configured"`, and
`transport_instance`. Slack-fetched display is intentionally omitted from model
identity. Native IDs remain model-visible; hiding them would require a separate
cross-transport protocol and authorization migration.

## Installation and verification

Startup and reconnect bind both the exact bot U/W ID and installing team ID from
`auth.test`. A supported Events API wrapper is eligible only when:

- its exact `context_team_id` matches the installation; or
- when that field is absent, it has exactly one authorization record for the
  installing team and, when present, the expected bot user.

Missing, malformed, ambiguous, or conflicting installation evidence fails closed
before `users.info`, local mutation, or ingress submission. Top-level event team
and the actor's home team are not installation authority. Slack Connect actors
may belong to a different home team.

Proactive sends made without a live worker installation observation perform a
read-only `auth.test` preflight before reservation. Accepted ingress, reply
routes, and frozen sends retain the exact installation team and revalidate it
against current state. App-token/bot-token same-app pairing remains an operator
configuration responsibility because the supported Slack APIs provide no
stronger proof without expanding scope.

Mutable configuration replacement discards any preflight pair before new
credentials can use it. Reconnect authentication must exactly match both halves
of the established pair. A changed, incomplete, or malformed reconnect
observation disables capability, retires
pending ingress, dynamic links/selections, reply/edit routes, posted-message
ownership, duplicate state, and all reaction targets/ownership, and requires a
Tau restart; it never reinterprets old native routes under the new installation.
That failure is a process-lifetime latch: pending capability correlation is
cleared, delayed acceptance cannot reactivate it, and later session/configuration
activity cannot request or accept capability again.
Every complete reconnect observation is compared immediately after `auth.test`
and before `apps.connections.open`, so a later ticket failure or invalid URL
cannot preserve authority after a valid changed pair was already observed.
The worker marks itself offline, stops rather than backing off, and emits exactly
one bounded categorical restart notice independent of ordinary startup-failure
suppression.
Already-started sends retain their existing completion/copy accounting but
cannot retry or install private authority after that failure.

Human verification is live per occurrence and uses one `users.info` response to
establish all of:

- exact response user ID equality;
- `deleted == false`;
- `is_bot == false`;
- `is_app_user == false`; and
- the optional display snapshot.

There is no identity cache and no extra display or mention-time lookup.

## Display and alias constraints

Only `profile.display_name` may become the Slack UI display. It is trimmed and
retained only when nonempty, at most 80 Unicode scalar values and 256 UTF-8
bytes, and free of control, bidi, zero-width/default-ignorable, line-separator,
and noncharacter structure. An invalid optional display is omitted without
changing an otherwise established human decision. Real name, email, deprecated
top-level name, title, workspace name, and actor team are not retained.

`sender_aliases` is an optional deny-unknown-fields list with at most 64 entries.
Each entry maps one exact valid U/W ID to one alias matching
`^[a-z][a-z0-9_-]{0,63}$`. User IDs and aliases are each unique, making the
mapping one-to-one. Configuration is frozen at startup; rotation affects only
newly accepted occurrences. Aliases do not create proactive destinations and
are not exposed by conversation discovery.

The generic protocol endpoint alias is accepted by the harness only with a
stable external ID, `VerifiedAccount` assurance, valid bounds/grammar, and the
closed `OperatorConfigured` authority. The harness, rather than the extension,
supplies transport-instance provenance.

## Persistence, duplicate handling, and replay

The accepted occurrence stores display and alias in the canonical endpoint.
They are durable, visible to event subscribers, and replayed without any Slack
API call, alias re-resolution, or profile refresh. Older records decode absent
optional fields as `None`.

Duplicate compatibility compares authority and operation fields while excluding
endpoint display, identity alias, and conversation display. A compatible retry:

- returns the original canonical message ID;
- preserves the first committed presentation snapshot;
- reconstructs live route presentation from that first canonical result; and
- does not wake or append another occurrence.

New edits and reactions are distinct occurrences and may carry a later display
snapshot.

## UI and reaction presentation

The CLI uses one transport-aware, single-line endpoint formatter. Slack humans
render stable ID first, followed by optional `Slack "display"` and `alias name`.
Quotes, backslashes, controls, bidi/default-ignorable characters, and other
structure are visibly escaped. Components truncate only on grapheme boundaries;
the final label is bounded to 512 bytes and 160 columns while preserving the
stable-ID/alias baseline.

Inbound Slack reactions render as one compact fact containing the exact actor,
Add/Remove action, and exact validated reaction name, for example:

```text
slack reaction removed by U123 (Slack "Alice"; alias dpc) · :thumbsup::skin-tone-6:
```

Custom names and the optional exact `::skin-tone-[2-6]` suffix are preserved.
No Unicode glyph lookup replaces the stable name. Typed incoming and outgoing
events are filed under their actual recipient or sender transcript, so live,
redraw, history, and restart replay use the same presentation.

Operational logs and categorical failures remain free of actor IDs, displays,
aliases, reaction names, message text, and installation identifiers.
