# SPEC-exact-sentinel-prompt-envelopes: Exact-close model-facing envelopes

## Record justification

Exact-close framing spans typed transcript projection, external and agent
messages, interactive prompts, hosted-provider results, system-prompt templates,
and provider assembly, so no one local artifact can own the complete contract.

Model-facing payload envelopes are exact lexical sentinels, not XML. Before
framing, the trusted projector normalizes and bounds the complete body, replaces
every byte-exact occurrence of that envelope's closing token with its fixed
visible form, and then appends the trusted closing token. No later normalization
or truncation may modify the framed result.

Replacement is case-sensitive and changes no other text. Opening tags, near
variants, other envelope families, entity-like text, and Unicode remain literal.
Dynamic attributes retain their separate validation and escaping rules.

The framed body therefore cannot emit its enclosing exact closing sentinel.
Nested, cross-family, and delimiter-like payload text does not change enclosing
source, role, trust, routing, tool, or instruction authority. This guarantee is
lexical framing, not XML well-formedness or semantic prompt-injection prevention.

Whenever selected context contains a governed envelope, prompt assembly supplies
this provenance rule to every system-prompt template.
