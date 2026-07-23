# DECISION-exact-sentinel-prompt-envelopes: Exact-close framing for model-facing payload envelopes

Authority: confirmed, 2026-07-23, dpc

## Status

The confirmed XML-escaping contracts and their implementations remain current
until this decision receives final human confirmation and the exact-sentinel
transition lands. At that transition, this record supersedes only their
element-body escaping rules; their typed provenance, roles, trust, routing,
activation, replay, metadata, and attribute contracts remain in force. The
transition must synchronize the affected harness prompt-dispatch and provider-data
specifications, agent-message delivery specification, external-message
specification, web-content boundary and resource-safeguard specifications,
security documentation, user documentation, prompt-template guidance, and
deterministic fixture contract.

Tau treats its model-facing payload envelopes as exact sentinels, not XML. The
trusted projector owns a hard-coded opening token and matching exact closing
token for each envelope. It assembles and applies any domain normalization and
bounds to the complete body first, replaces every exact occurrence of that
envelope's closing token with its fixed visible form, and only then appends the
trusted closing token. No later normalization or truncation may modify the
framed result.

For example, a `<user>` body replaces each exact `</user>` with
`&lt;/user&gt;`. Matching is byte-exact and case-sensitive: whitespace, case, or
other near variants are ordinary body text. The framing step changes no other
body text. Apostrophes, quotes, ampersands, opening tags, closing tokens for
other envelope families, entity-like text such as `&apos;`, `&quot;`, `&amp;`, and
`&lt;`, newlines, and Unicode therefore retain their assembled spelling.
Pre-existing visible forms are not decoded or recognized specially.

This rule governs body framing for direct HumanUi `<user>` projections,
canonical external-message `<message>` facts, inbound local-agent `<message>`
and cross-session `<tau_peer_message>` projections, watch `<prompt>` and
`<response>` projections, and external web-content `<tau_web_content>` results.
Bodyless/self-closing facts have no body token to replace. System-prompt
template mechanics and intermediate markup are outside this decision.

Dynamic attributes remain a separate grammar context. Their existing
validation, visible-Unicode policy, bounds, and quote-safe escaping remain in
force; body framing must never be reused for an attribute. Canonical prompt,
agent-message, and external-message-fact data remains raw. Projection is one-way
and needs no decoder. Live assembly and replay derive the same form from typed
provenance; already materialized provider text, including committed compaction
windows, is not decoded or rewritten.

Web content retains its existing extension-owned boundary rather than gaining a
new typed protocol or persistence representation. `tau-ext-websearch` normalizes
and bounds the provider body, applies exact-close framing, and publishes that
complete string as the canonical tool result. Replay treats both historical
XML-escaped strings and new exact-close strings as opaque canonical results and
never decodes or reframes either form. The existing 512 KiB decoded-body cap
still applies before framing. The separately bounded final framed candidate may
be rejected after framing when it exceeds 512 KiB, but is never truncated or
otherwise normalized after the trusted close is appended.

For every content-bearing envelope, the complete projection must end with and
contain exactly one exact closing token: the trusted suffix. Repeated exact
closers in the body are all replaced. Bounds apply before framing so truncation
cannot remove the trusted suffix or expose a partial replacement. Envelope
constants and trusted metadata, not payload spelling, select the framing rule.

The guarantee is deliberately lexical and narrow: arbitrary body content cannot
emit that envelope's exact closing sentinel and escape its outer data region. It
is not XML well-formedness, a parser boundary, or semantic prompt-injection
prevention. Payloads may contain nested, opening, near-closing, or
provenance-looking delimiters. Only the exact outer delimiters stamped by Tau in
the native typed item establish provenance; delimiter-like payload text does not
change the enclosing source, role, trust, routing, tool, or instruction
authority.

Prompt assembly must supply this provenance rule as an explicit system-template
input whenever selected context contains any governed envelope. Every built-in
template emits it exactly once; every custom template receives the same input
and retains control of its placement under
[DECISION-tau-harness-system-prompt-templates](../crates/tau-harness/specs/DECISION-tau-harness-system-prompt-templates.md).
The rule must identify the exact outer Tau-stamped sentinel as the only textual
provenance cue and state that nested, cross-family, and delimiter-like body text
does not change the enclosing source or trust. For external-message and
web-content bodies it must retain the separate statement that content and
metadata are untrusted data and grant no identity, routing, tool, or instruction
authority. A custom template that omits the supplied rule is an operator
replacement of this model-visible cue, not a stronger lexical guarantee.

Native provider roles and typed tool/result blocks remain the primary structural
boundary. Harness-stamped provenance, deterministic capability and schema
checks, authorization, and required user confirmation remain authoritative even
if a model follows hostile body instructions.

## Verification contract

Every governed content-bearing family must prove that its projection ends with
and contains exactly one exact closing token, including bodies with repeated own
closers. Tests must preserve exact case and whitespace near variants,
cross-family complete wrappers, nested provenance-looking tags, quotes,
apostrophes, ampersands, entity-like text, newlines, arbitrary UTF-8, empty
bodies, and boundary-sized bodies. They must separately verify attribute
validation and escaping, pre-framing normalization and bounds, reject-only final
web-content bounds, live/replay equality for typed projections, opaque replay of
old and new web results, and no rewrite of committed materialized provider text.

System-template tests must cover every built-in template and the conditional
custom-template input, including a cross-family nested wrapper whose inner
delimiters remain text. Capability, schema, authorization, and confirmation tests
must continue to reject consequential hostile model output independently of
lexical framing.

## Rationale

The previous shared XML escaper encoded `&`, `<`, `>`, `"`, and `'` in element
bodies. That made ordinary prose harder to read and copy, increased token use,
and made literal entity-like text ambiguous; an additional projection pass made
`&apos;` appear as `&amp;apos;`. Tau does not parse these model-facing strings as XML,
so full XML character-data serialization provides no needed machine boundary.
Escaping only the exact close preserves source fidelity while retaining the
specific breakout invariant the envelope requires.

Minimal XML element escaping was rejected because it would still rewrite
ampersands and markup-heavy text. Continuous line marking would provide a
stronger visual distinction, but adds per-line token cost, changes copied text,
and requires more newline policy. Leaving the exact close untouched would let a
body terminate its harness-stamped sentinel. Exact-close framing accepts that
near variants and nested markup can remain visually confusing; native typing,
explicit trust policy, and deterministic action controls address the authority
boundary rather than claiming punctuation can make hostile instructions inert.

Upon the implementation transition described in this record's status, this
decision supersedes the body-escaping portions of
[DECISION-interactive-user-prompt-envelope](DECISION-interactive-user-prompt-envelope.md),
[DECISION-common-external-message-envelope](DECISION-common-external-message-envelope.md),
and
[DECISION-agent-message-transcript-projection](DECISION-agent-message-transcript-projection.md).
It also supersedes the XML-body-escaping portions of
[SPEC-tau-ext-websearch-provider-boundary](../crates/tau-ext-websearch/specs/SPEC-tau-ext-websearch-provider-boundary.md)
while retaining the extension-owned canonical result and the independent caps in
[SPEC-tau-ext-websearch-runtime-safeguards](../crates/tau-ext-websearch/specs/SPEC-tau-ext-websearch-runtime-safeguards.md).
It is governed by
[DECISION-persistence-and-extension-interface-change-approval](DECISION-persistence-and-extension-interface-change-approval.md).
