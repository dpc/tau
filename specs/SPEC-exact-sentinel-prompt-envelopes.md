# SPEC-exact-sentinel-prompt-envelopes: Generic user-payload envelope contract

## Record justification

Payload-envelope behavior spans protocol provenance and rendering, core transcript
folding, harness user-role projection, agent/watch/external inputs, compaction and
replay, and provider lowering, so no one local artifact can own the contract.

## Prospective scope

[GATE-new-generic-user-payload-envelopes](GATE-new-generic-user-payload-envelopes.md)
requires every new free-form payload kind, and every reframed existing kind, in the
shared generic user-role `ContentPart::Text` carrier to use exactly one registered
top-level XML-lite family. The compliance unit is one complete text part after
shared prompt assembly and before provider lowering; unrelated text cannot sit
outside its outer envelope.

Typed non-message context items remain self-identifying. Ordinary foreground tool
results, harness-synthetic background placeholders, and results returned by `wait`
remain `ToolResultItem`s and need no additional generic-user envelope. Other typed
content parts, the system/developer prompt channel, non-model text, assistant-role
facts, and immutable provider-native replay items are also outside the gate.
Existing legacy projections remain unchanged until separately approved; unrelated
maintenance does not trigger migration.

## Registered outer families

`tau_proto::registered_payload_envelopes` is the central registry for top-level
model-facing payload families:

| Family | Carrier | Opening schema |
| --- | --- | --- |
| `user` | Generic user-role text | Fixed `source="ui"`, then optional `delayed="true"`; direct Tau user-channel input |
| `extension_output` | Generic user-role text | `publisher`, `operation_id`; passive configured-extension output |
| `discovery` | Generic user-role text | Fieldless; typed current discovery scope and content versions absent from surviving context receipts |
| `agent_update` | Generic user-role text | Fieldless; typed agent, kind, status/task or observed prompt/response |
| `agent_message`, `peer_message`, `cli_message` | Generic user-role text | Fieldless; source-specific identity, reply policy and body |
| `agent_context`, `tool_completion`, `compaction_result` | Generic user-role text | Fieldless; bootstrap or typed lifecycle notice, never a second tool payload |
| `context_size_alert`, `loop_guard`, `status_reminder`, `session_notice`, `tool_availability`, `output_continuation`, `compaction_recall` | Generic user-role text | Fieldless; meaning-specific harness instructions and notices |
| `summary_request`, `summary_continuation` | Generic user-role text | Fieldless; provisional local-summary dialogue |
| `message` | Canonical user- or assistant-role text, selected by event direction | Canonical ordered message-fact attributes, including operation-only `message_ref_publisher`, optional `sender_trust`, and optional descriptive JSON `mentions` before `content_trust` |
| `quoted_message` | Generic user-role text attached to a canonical delivered/edited message | Optional descriptive reference, author, and excerpt metadata; `content_trust="external"` |
| `tau_web_content` | Typed tool result | `adapter`, `operation`, `content_trust` |
| `tau_background_result` | Generic user-role text | Historical canonical background-preview attributes |
| `tool_output` | Generic user-role text | `call_id`, `terminal`, optional terminal `status`, then nonzero `omitted_bytes`; ordered canonical tool source |

New injected-user projections use unprefixed, meaning-specific names.
`tau_internal`, `tau_extension_output`, `tau_discovery`, and
`tau_background_result` remain registered for historical recognition.
The typed web adapter uses `tau_web_content` until its separate conversion to
canonical tool-result headers and payload. It is not an injected-user producer.
The shared `ToolResponse` serializer renders semantic headers before outer framing;
header metadata also retains source-specific visible escaping.

Each change that adds or reframes a generic-user family must register it and use
the registry's shared metadata and exact-close helper. Family renderers own
their typed source selection, validated attributes, normalization, and complete-body
bounds; the registry owns the stable tag, opening shape, attribute order, exact
close, visible close, carrier classification, and whole-envelope recognition.

## Nested delimiters

Agent-message `<message>`, peer `<tau_peer_message>`, watch `<prompt>` and
`<response>`, skill, AGENTS/activity, and similar payload-local markup may structure
an enveloped body but establishes no independent Tau provenance. Components must
not describe or rely on local tag-shaped text as a trust or provenance boundary.
The family-specific schemas remain in
[SPEC-agent-message-delivery](SPEC-agent-message-delivery.md),
[SPEC-agent-watch](SPEC-agent-watch.md), and
[SPEC-interactive-user-prompt-envelope](SPEC-interactive-user-prompt-envelope.md).
Passive configured output follows
[SPEC-passive-extension-context](SPEC-passive-extension-context.md).

## Selection, attributes, and exact-close framing

Typed event or harness provenance selects a family; parsing or matching payload text
never grants envelope authority. Dynamic attributes come only from validated typed
metadata, use deterministic full XML attribute escaping, and follow the registry's
order. Payload cannot inject attributes.

Before framing, the trusted renderer normalizes and bounds the complete body,
replaces every byte-exact occurrence of that family's closing token with its fixed
visible form, and appends the trusted closing token. No later normalization or
truncation may modify the result. Replacement is case-sensitive and changes no
other text. Opening tags, near variants, other families, Markdown, JSON,
entity-shaped text, and Unicode remain literal unless the source contract defines a
separate visible-normalization rule.

XML-lite framing is lexical, not XML parsing or semantic prompt-injection
prevention. An envelope adds no Tau identity, routing, reply, tool, egress, sender,
or instruction authority and does not strengthen or weaken the source's existing
request and trust semantics. `content_trust="external"` remains a closed marker for
external network content; its absence never means trusted.

The `user` family's `source="ui"` attribute names the accepted Tau user-channel
submission class. It retains normal user-request semantics but does not attest
personal authorship or a particular CLI executable.

The built-in XML-shaped skill catalog is presentation metadata, not an exact-close
payload envelope. Its `xml_escape_lax` formatter replaces every literal `</` prefix
with `&lt;/` and preserves every other byte.

## Projection and replay

Durable `agent.prompt_submitted` and `agent.prompt_steered` facts carry
`trusted_internal_spans`; prompt assembly frames only authenticated ranges as
`HarnessInternalText`, while an absent list projects ordinary payload. The
in-process `agent_start` path may attach equivalent transient spans, which the
harness copies to the child prompt fact; configured extensions cannot assert them.
New spans carry an optional typed `SemanticEnvelope` kind, propagated into the
context part. Missing kinds retain the historical internal projection. No text
parser guesses a kind. Internal lifecycle classification alone, including timer
and configured UI content, never grants this presentation authority.

Each durable provider-visible tool terminal carries `ToolResultPresentation`.
Ordinary `tool_payload` remains in the typed result channel with an exact
semantic close collision neutralized. A harness-stamped `harness_dedup_pointer`
projects as canonical tool text with a `status: duplicate` header, not XML;
configured extensions submit the default presentation. Compaction retains the
typed discriminators.

Canonical message facts retain raw typed data and render through the shared
`message` family. Optional typed native reply context adds relation metadata to
that part and, when text is available, one adjacent `quoted_message` text part in
the same `MessageItem`. The quote adds descriptive context, not a second sender
event or activation. Each part follows the one-outer-family rule; payload-local
tags cannot produce either part.
Provider adapters preserve assembled envelope bytes and do not synthesize, remove,
or reinterpret a family. Stored raw facts, committed text, provider replacement
windows, and opaque replay bytes are not rewritten. On live append and cold replay,
derived projections of typed agent-message facts use the current semantic families.
Preserving stored bytes does not promise historical derived XML.
Historical fieldless `<user>...</user>` replacement bytes remain recognizable
for provenance-notice delivery only; recognition does not infer or reproject a
typed source.
Cross-session agent messages use authenticated `peer_message` framing with
source identity in headers. When either persisted configured notice exists, the renderer exact-close-frames
sender- and recipient-origin advisory `<notice>` nodes separately from the original
`<message>` body. Nested notice/message tags establish no generic envelope family
or authority. These notice snapshot, wire, rendering, bounds, mixed-version,
and replay semantics were explicitly approved by the user on September 12,
2026.

Historical background-preview facts retain the `tau_background_result` envelope
rendered from typed background-terminal and call correlation. Its
ordered attributes authenticate call identity, tool name, logical tool outcome,
full-versus-summary delivery, canonical rendered-body byte count, retrieval, and
optional bounded summary metadata. The complete body remains untrusted tool data.
The prompt keeps internal lifecycle classification but carries no trusted internal
span, so provider projection preserves the committed generic-user envelope without
adding an outer
`tau_internal`. Replay preserves the committed bytes and never selects this family
from text spelling. `AgentTree` rebuilds a non-serialized exact-node index only from
committed submitted or steered prompt facts tagged
`internal_kind=background_tool_completion`; deferred inference and open-tool-round
placement carry that fact to the eventual node. A compaction replacement never gains
this authority, while a retained suffix node keeps it. Provider projection preserves
the authoritative node's exact bytes and uses this registry family's close escaping
to prevent every other generic text item from forming a complete
`tau_background_result` envelope.

New output uses `tool_output`, rendered only from accepted canonical updates
or the final canonical terminal. The exact declaration reference governs source
ownership; text spelling cannot confer it. The harness retains final source before
native closure, and completion notices and waits carry control receipts rather than
another payload copy. The harness preserves canonical source bytes through
deferred placement and replay, sanitizes provider controls, and uses the registry's
exact-close escaping for the untrusted body. See
[SPEC-streamed-tool-output](SPEC-streamed-tool-output.md).

The harness uses this envelope to frame injected context. The producer supplies
ordinary tool text. Its header/payload format belongs to
[SPEC-tau-proto-provider-data](../crates/tau-proto/specs/SPEC-tau-proto-provider-data.md);
native typed tool results do not need this generic-user carrier.

These durable typed-span and tool-presentation semantics have the explicit approval
required by
[GATE-persistence-and-extension-interface-change-approval](GATE-persistence-and-extension-interface-change-approval.md).
The `tau_background_result` preview, replay, and framing semantics were explicitly
approved by the user on September 5, 2026.
The registry itself remains descriptive metadata; each owning renderer and its
typed source semantics control event, persistence, replay, and interface behavior.

## Provenance notice

Whenever selected context contains a registered envelope, prompt assembly supplies
the model-visible provenance rule to every system-prompt template. The notice derives
its top-level family list from `registered_payload_envelopes`; nested
`tau_peer_message`, `prompt`, and `response` delimiters are not listed as outer
families. Discovery detection uses canonical typed delivery provenance, including
the initial preview, rather than a replaceable AGENTS initialization prefix.
