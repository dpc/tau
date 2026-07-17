# DESIGN-tau-ext-slack-transport-identity-mentions: Recognize the Slack bridge identity

Status: confirmed, 2026-07-15, dpc

**Bead:** `tau-agent-d630`
**Related:** [DESIGN-tau-ext-slack-sender-identity](DESIGN-tau-ext-slack-sender-identity.md),
[DESIGN-tau-ext-slack-sender-admission](DESIGN-tau-ext-slack-sender-admission.md),
and [DESIGN-tau-ext-slack-single-agent-operating-model](DESIGN-tau-ext-slack-single-agent-operating-model.md)

## Decision

Incoming Slack text may refer to the authenticated receiving identity with the
exact semantic token `@slack_bridge`. This means the bot identity shared by one
Slack extension installation, not the selected Tau agent and not a native Slack
identifier. Multiple prefixed instances use the same token and remain
distinguishable through the harness-stamped `transport_instance`.

The generic canonical envelope carries a default-false
`transport_identity_mentioned` fact. The true case projects to provider XML as
`transport_identity_mentioned="true"`. It is informational only and grants no
command, routing, selection, reply, send, reaction, allowlist, or dynamic-DM
authority. The typed fact, rather than the literal token, is the attestation.

## Detection and normalization

Classification uses only the exact validated `auth.test.user_id` paired with the
admitted event's installation team and lifecycle epoch. It recognizes exact,
case-sensitive `<@U…>`/`<@W…>` entities for that identity in bounded decoded
Slack text. Complete equal-length inline or fenced backtick ranges suppress
recognition; an unmatched backtick is literal and does not.

Tau does not HTML-decode before matching. Escaped, labeled, partial, malformed,
case-changed, lookalike, other-user, and literal `@slack_bridge` text do not set
the fact. Event kind alone is not evidence.

Every eligible non-leading occurrence becomes literal `@slack_bridge` in
canonical logical text. Exactly one eligible leading occurrence is removed,
preserving established command/routing-prefix behavior while retaining the true
typed fact. `to <agent> <body>` carries the original fact into the routed body.
Bridge-local commands produce no ingress. Creates and edits classify their own
current text; reactions and deletes are false.

Threads, parent/fixed routes, channels, MPIMs, static DMs, and dynamic DMs use
the same classifier. Queued work cannot reclassify under a replacement
installation because installation mismatch is terminally fail-closed.

## Durability and duplicate behavior

The fact and normalized operation are immutable occurrence content. Dual wrappers
share one Slack-local native message-id cache key and recent repeats are dropped
before submission. A repeat admitted after cache eviction or restart creates a
new occurrence. Old envelopes decode absence as false, and replay uses each
stored value without Slack lookup or reparsing.

## Registration and egress

Successful `slack_register(enabled:true)` returns:

```json
{"status":"registered","incoming_transport_reference":"@slack_bridge"}
```

Unregister returns only `{"status":"unregistered"}`. This is static orientation,
not a selector, identifier, bearer value, or capability.

No model-facing result or field exposes the bot U/W/B or team ID. No API accepts
the semantic token as authority. On egress `@slack_bridge` remains literal and
is never expanded. Raw `<@`, `<!`, and `<#` controls remain rejected and
`mention_source_user` remains the only generated native user mention.

Logs, notices, diagnostics, traces, and tool results do not copy private
installation identifiers or hostile message bodies.

## Verification ownership

Slack unit/integration tests own exact entity parsing, code-range negatives,
create/edit/command/route behavior, registration maps, reactions, and literal
egress. Protocol tests own legacy CBOR defaults, round trips, and true/false XML
projection. Harness tests own operation validation. Slack tests own native-id
cache suppression and bounds. Prompt tests
own the model-visible non-authority explanation. Full workspace CI owns
cross-crate constructor and serialization compatibility.
