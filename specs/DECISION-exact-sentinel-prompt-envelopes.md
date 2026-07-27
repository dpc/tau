# DECISION-exact-sentinel-prompt-envelopes: Exact-close framing for model-facing payload envelopes

Authority: confirmed, 2026-07-23, dpc

## Decision

Tau treats model-facing payload envelopes as exact sentinels, not XML. For
each envelope, the trusted projector normalizes and bounds the complete body,
replaces every byte-exact occurrence of that envelope's closing token with its
fixed visible form, and then appends the trusted closing token. No later
normalization or truncation may modify the framed result.

The projector changes no other body text. Matching is case-sensitive, and
opening tags, near variants, other envelope families, entity-like text, and
Unicode remain unchanged. Dynamic attributes retain their separate validation
and escaping rules.

The resulting guarantee is lexical: body content cannot emit its enclosing
envelope's exact closing sentinel. Nested, cross-family, and delimiter-like
payload text does not change the enclosing source, role, trust, routing, tool,
or instruction authority. This is not XML well-formedness or semantic
prompt-injection prevention.

Prompt assembly supplies that provenance rule to every system-prompt template
whenever selected context contains a governed envelope, under
[DECISION-tau-harness-system-prompt-templates](../crates/tau-harness/specs/DECISION-tau-harness-system-prompt-templates.md).

## Rationale

Full XML escaping rewrote ordinary prose, increased token use, and made literal
entity-like text ambiguous even though Tau does not parse these model-facing
strings as XML. Exact-close framing preserves source fidelity while retaining
the required breakout invariant. Native provider typing, authorization, schema
checks, and confirmation remain the primary action boundaries.

This decision supersedes the body-escaping portions of
[DECISION-interactive-user-prompt-envelope](DECISION-interactive-user-prompt-envelope.md),
[DECISION-common-external-message-envelope](DECISION-common-external-message-envelope.md),
and
[DECISION-agent-message-transcript-projection](DECISION-agent-message-transcript-projection.md).
It also supersedes the XML-body-escaping portion of
[SPEC-tau-ext-websearch-provider-boundary](../crates/tau-ext-websearch/specs/SPEC-tau-ext-websearch-provider-boundary.md).
