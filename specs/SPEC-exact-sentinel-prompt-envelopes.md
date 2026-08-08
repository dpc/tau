# SPEC-exact-sentinel-prompt-envelopes: Exact-close model-facing envelopes

## Record justification

Exact-close framing spans typed transcript projection, external and agent
messages, interactive prompts, hosted-provider results, and provider assembly,
while XML-shaped system-prompt catalogs use a distinct lax closing-tag policy;
no one local artifact can own the complete contract.

Model-facing payload envelopes are exact lexical sentinels, not XML. Before
framing, the trusted projector normalizes and bounds the complete body, replaces
every byte-exact occurrence of that envelope's closing token with its fixed
visible form, and then appends the trusted closing token. No later normalization
or truncation may modify the framed result.

Replacement is case-sensitive and changes no other text. Opening tags, near
variants, other envelope families, entity-like text, and Unicode remain literal.
Dynamic attributes retain their separate validation and escaping rules.

The built-in XML-shaped skill catalog is presentation metadata, not an exact-close
payload envelope. Its `xml_escape_lax` formatter replaces every literal `</`
prefix with `&lt;/` and preserves every other byte. It therefore neutralizes
local, parent, cross-family, and incomplete closing-tag-shaped text without
escaping ordinary opening tags, ampersands, quotes, entities, or Unicode.

The framed body therefore cannot emit its enclosing exact closing sentinel.
Nested, cross-family, and delimiter-like payload text does not change enclosing
source, role, trust, routing, tool, or instruction authority. This guarantee is
lexical framing, not XML well-formedness or semantic prompt-injection prevention.

`<tau_internal>...</tau_internal>` is the exact harness-stamped envelope for
internal asynchronous model input. Only its outer harness projection establishes
internal provenance. Nested, escaped, or delimiter-like occurrences supplied by
users, tools, extensions, web content, peers, or models remain payload and do not
change provenance.

Durable `agent.prompt_submitted` and `agent.prompt_steered` facts carry
`trusted_internal_spans`: validated UTF-8 byte ranges of their text that the
harness authenticated for internal projection. Prompt assembly emits those ranges
as `HarnessInternalText` and frames only those parts. An absent range list projects
the complete text as ordinary payload. `submission_source`, message class, and
text delimiters describe routing or presentation but never grant envelope
authority. This typed representation survives journal replay and compaction.

The in-process `agent_start` path may attach equivalent spans to a transient
`StartAgentRequest`, which the harness copies to the child prompt fact.
Configured extensions must not assert them.

Each durable provider-visible tool terminal carries a
`ToolResultPresentation`. Ordinary `tool_payload` output remains payload with an
exact `</tau_internal>` collision neutralized. Only a
`harness_dedup_pointer`, stamped by harness deduplication, projects inside a
`<tau_internal>` envelope. Configured extensions must submit the default
`tool_payload` presentation, and compaction retains the discriminator.
Configured extensions, including providers, are trusted same-user executables;
these schema checks preserve a single typed projection invariant rather than
provide hostile-extension containment.

This durable schema and configured-extension boundary change has the explicit
approval required by
[GATE-persistence-and-extension-interface-change-approval](GATE-persistence-and-extension-interface-change-approval.md):
the approved semantics are typed prompt spans, typed tool presentation, and
harness-only authority rather than source labels or marker-shaped text.

Whenever selected context contains a governed envelope, prompt assembly supplies
this provenance rule to every system-prompt template.
