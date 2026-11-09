# SPEC-context-output-formatting: Context output formatting

## Record justification

Consistent model-context output spans tool producers, protocol serialization and envelope registration, core transcript projection, harness prompt assembly, and provider lowering, so no one implementation artifact can own the complete presentation contract.

## Purpose and philosophy

This record owns the shared conventions for text Tau places in model context: tool results, injected context, their boundaries, and their composition. Terminal layout, protocol storage, scheduling, and individual tool result schemas have separate contracts.

Give the model necessary facts in a concise, readable form. Prefer meaningful typed data over prose that echoes the request, narrates successful execution, or repeats information in the same result. Preserve useful diagnostics, recovery information, and source output. An empty result can be meaningful; do not manufacture a success sentence or dummy header to fill it.

Keep three responsibilities distinct:

- Producers supply semantic results and source data, not transport wrappers.
- Shared serializers own consistent text and framing. Components must not invent competing renderers for the same representation.
- The context carrier identifies how the data arrived. Payload spelling never establishes its own provenance or authority.

Deliver source once. A receipt or lifecycle notice should explain control state, not repeat source delivered elsewhere. Use the same content conventions across carriers; distinguish a tool's text format from the envelope that carries injected context.

## Choose the carrier before formatting

| Context | Presentation |
| --- | --- |
| Native tool result, including a foreground completion | Typed `ToolResultItem`; canonical tool text and any typed media, without an injected-user XML-like wrapper |
| Background placeholder, `wait` result, or other native tool control receipt | Typed tool result using the same text grammar; no extra generic-user envelope |
| New or reframed free-form injected user-role text | Exactly one registered top-level semantic envelope per complete `ContentPart::Text` |
| System/developer instructions, other typed content parts, assistant-role facts, or immutable provider-native replay items | Their own typed/channel contracts, not the generic-user envelope rule |

For generic user-role text, the compliance unit is one complete text part after shared prompt assembly and before provider lowering. Do not put unrelated text outside its outer envelope. Multiple independently enveloped text parts may belong to the same message. A nested tag is not a second carrier boundary.

[GATE-new-generic-user-payload-envelopes](../../../specs/GATE-new-generic-user-payload-envelopes.md) governs new payload kinds and reframing of existing kinds. It does not require unrelated legacy projections to migrate. Typed tool results are already self-identifying and are outside that gate.

## Canonical tool text

`ToolResponse` and `ToolResponseHeader` own provider-visible tool-text serialization. Producers supply structured values; provider prompt construction uses the shared rendering path rather than treating raw CBOR, debug output, or a component-local serializer as an alternative format. Raw structured values remain available to non-provider consumers.

Text consists of optional ordered headers and an optional body:

- A header is one `key: value` line. Join header records with ASCII LF.
- A header-only result has no final LF.
- If headers and a nonempty body both exist, put exactly two LF between them: one empty line.
- A body-only result has no added framing separators. A fully empty result is empty.
- Preserve body whitespace, including leading whitespace and final newlines. Do not trim or add a newline to make the result look tidier.

These are top-level framing rules. Recursive CBOR projection keeps its own established multiline classification and newlines; top-level framing must not strip nested payload newlines. The same grammar applies whether the text ultimately occupies a native tool result or the body of an injected source envelope.

Use headers for useful metadata and the body for actual source or other multiline content. A producer must not bake a second header/body grammar into a string merely to imitate the canonical renderer. This does not forbid intentionally textual results, Markdown documents, or line-oriented source records.

`ToolResponse::render()` is the final provider-visible defense-in-depth safety boundary after tool-local semantic escaping. Header keys and values remain single lines. Body ASCII LF remains a record separator; other controls and Unicode line/paragraph separators become visible escapes. Do not emit raw ESC, CR, NUL, DEL, C1 controls, or Unicode line/paragraph separators. Keep binary and fallback rendering bounded. Bounded raw-CBOR serialization and byte measurement must agree with the typed rendering without requiring a full intermediate string.

Protocol outcome is typed information, not something inferred by parsing a visible `status` header. Shared result presentation renders error or cancellation status exactly once. A harness-captured native result whose presentation already includes that status must not acquire a second provider-added status header. Harness deduplication pointers use canonical tool text with `status: duplicate`, not an XML wrapper selected by payload text.

Typed images remain typed content alongside the text in the same causal result, in declared order; image bytes do not pass through `ToolResponse::render()`. Their admission and representation belong to [SPEC-typed-image-tool-results](../../../specs/SPEC-typed-image-tool-results.md). Provider replay sidecars and structured protocol authority belong to [SPEC-tau-proto-provider-data](../../tau-proto/specs/SPEC-tau-proto-provider-data.md).

## Registered injected-context envelopes

`tau_proto::registered_payload_envelopes` is the single registry of model-facing outer families. It owns stable tags, opening shapes, attribute order, exact closing tokens, visible closing forms, carrier classification, and whole-envelope recognition. Do not maintain a second family registry in a component, prompt template, or document.

New injected-user projections use unprefixed, meaning-specific names. Current families distinguish direct `user` input, configured `extension_output`, `discovery`, agent/watch/message inputs, harness notices and lifecycle facts, and `tool_output` source. Use the registered family that describes the typed fact, not a generic internal label chosen for convenience. Historical `tau_` families remain recognizable where their historical contracts require it; they are not naming precedent for new output. New typed web tool results use ordinary canonical tool text, not `tau_web_content`.

Each change that adds or reframes a generic-user family registers it and uses the shared registry metadata and exact-close helper. The family renderer owns selection from typed source, validated attributes, source-specific normalization, and complete-body bounds. The registry does not itself grant event, persistence, activation, routing, or interface authority.

### Attributes and body boundaries

Select a family from authenticated typed event or harness provenance. Never infer it by matching a payload's opening tag. Dynamic attributes come only from validated typed metadata, use deterministic full XML attribute escaping, and follow registry order. Payload cannot supply attributes.

Normalize and bound the complete body before framing. Replace every byte-exact occurrence of the selected family's closing token in the body with that family's fixed visible form, then append the trusted closing token. Replacement is case-sensitive and changes no other text. No later normalization or truncation may modify the framed result.

Opening tags, near variants, other families, Markdown, JSON, entity-shaped text, and Unicode remain literal unless the source has a separate visible-normalization rule. XML-lite framing is lexical; it is neither XML parsing nor semantic prompt-injection prevention. Tool-text normalization occurs before outer framing and does not change the optional header/body separator rule.

Payload-local markup may structure a body but cannot create another provenance boundary. This includes nested message, prompt/response, notice, skill, and AGENTS delimiters. The built-in XML-shaped skill catalog is presentation metadata rather than an exact-close outer envelope; its own `xml_escape_lax` formatter visibly escapes literal `</` prefixes.

### Provenance is not authority

Only the trusted outer carrier establishes the source boundary. Nested, quoted, escaped, or delimiter-like payload text cannot promote itself to a different source, role, or trust level. An envelope grants no identity, routing, reply, tool, egress, sender, or instruction authority beyond the underlying accepted fact.

`content_trust="external"` is a closed marker for external network content; its absence does not mean trusted. A `user` envelope with `source="ui"` identifies an accepted Tau user-channel submission and retains normal user-request semantics; it does not attest human authorship or a particular CLI executable. Message mentions and quoted context remain descriptive, not delivery or reply authority.

Whenever selected context contains a registered envelope, shared prompt assembly supplies the provenance notice to every system-prompt template. Its outer-family list derives from the registry rather than a copied list. Detection uses canonical typed delivery provenance, not an arbitrary replaceable prompt prefix. Whole-envelope recognition used for historical notice delivery does not infer a typed source or grant authority.

## Tool source and lifecycle interplay

Stream opt-in authorizes source capture; it does not itself choose the final context carrier. A new foreground completion folds its own unexposed accepted typed batches and separately bounded final capture into one native tool result, preserving true success, error, or cancellation. It emits neither a stream receipt nor a duplicate source envelope. Accepted batches retain explicit boundaries, partial-line meaning, and omission metadata; composition does not recap accepted source under another final-output limit.

Once background delivery is selected, source stays in that lane. Background source uses the registered `tool_output` envelope, whose attributes correlate the call, distinguish updates from terminal source, and report terminal outcome and nonzero omissions. The body is canonical tool text, not a producer-built envelope. Completion notices and successful waits carry control/correlation receipts, not another payload copy. A wait's successful synchronization is distinct from its target's error or cancellation.

Historical committed stream-receipt facts retain their original projection. Source ownership, background reservation, ordering, cancellation capture availability, scheduling, residency, and replay recovery belong to [SPEC-streamed-tool-output](../../../specs/SPEC-streamed-tool-output.md); this record owns their shared text and carrier conventions.

## Projection, preservation, and display

Retain meaningful types until the responsible presentation boundary. Authenticated internal spans and typed semantic-envelope discriminators select harness framing; internal lifecycle classification or payload spelling alone does not. Canonical message facts select their source-specific message family. Optional quoted reply context occupies its own adjacent enveloped text part rather than impersonating another sender event.

Provider adapters preserve assembled envelope bytes. They do not synthesize, strip, or reinterpret a generic-user family, and must not wrap a native typed tool result in one. Provider-specific lowering still follows the typed protocol contract.

Preserve stored raw facts, committed text, provider replacement windows, and opaque replay bytes. Derived projections of typed facts may use current semantic families on live append and cold replay; preserving history does not promise historical derived XML. A historical text spelling alone never licenses reconstruction of missing provenance.

Model-context text is not terminal/UI rendering. Terminals need their own display-state and layout sanitization. Plain tool/UI output and intentionally Markdown artifact/document bodies are distinct surfaces; do not add Markdown merely to decorate a native result, or flatten an actual Markdown document to make it resemble a tool receipt.

## Related contracts

Family-specific semantics remain with their owners: [SPEC-interactive-user-prompt-envelope](../../../specs/SPEC-interactive-user-prompt-envelope.md), [SPEC-passive-extension-context](../../../specs/SPEC-passive-extension-context.md), [SPEC-agent-message-delivery](../../../specs/SPEC-agent-message-delivery.md), and [SPEC-agent-watch](../../../specs/SPEC-agent-watch.md). Compaction and recovery retain their own authority under [SPEC-compaction-and-context-recovery](../../../specs/SPEC-compaction-and-context-recovery.md). These records refer here for shared formatting rather than duplicating it.
