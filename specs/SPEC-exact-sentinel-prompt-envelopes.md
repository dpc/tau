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
| `user` | Generic user-role text | Fieldless; authenticated interactive UI prompt |
| `tau_internal` | Generic user-role text | Fieldless; typed harness-internal projection |
| `message` | Canonical user- or assistant-role text, selected by event direction | Canonical ordered message-fact attributes |
| `tau_web_content` | Typed tool result | `adapter`, `operation`, `content_trust` |
| `tau_background_result` | Generic user-role text | Canonical background-preview attributes |

Every new or reframed generic-user family must be added to the registry and use its
shared metadata and exact-close helper in the same change. Family renderers own
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

The built-in XML-shaped skill catalog is presentation metadata, not an exact-close
payload envelope. Its `xml_escape_lax` formatter replaces every literal `</` prefix
with `&lt;/` and preserves every other byte.

## Projection and replay

Durable `agent.prompt_submitted` and `agent.prompt_steered` facts carry
`trusted_internal_spans`; prompt assembly frames only authenticated ranges as
`HarnessInternalText`, while an absent list projects ordinary payload. The
in-process `agent_start` path may attach equivalent transient spans, which the
harness copies to the child prompt fact; configured extensions cannot assert them.

Each durable provider-visible tool terminal carries `ToolResultPresentation`.
Ordinary `tool_payload` remains in the typed result channel with an exact
`tau_internal` close collision neutralized. Only a harness-stamped
`harness_dedup_pointer` projects inside `tau_internal`; configured extensions submit
the default presentation. Compaction retains both typed discriminators.

Canonical message facts retain raw typed data and render through the shared
`message` family. Provider adapters preserve assembled envelope bytes and never
synthesize, remove, or reinterpret a family. Historical raw/default projections,
provider replacement windows, and payload-local wrappers retain their existing
replay behavior until a separately approved migration.

The harness renders a complete `tau_background_result` envelope from typed
background-terminal and call correlation before publishing its prompt fact. Its
ordered attributes authenticate call identity, tool name, logical tool outcome,
full-versus-summary delivery, canonical rendered-body byte count, retrieval, and
optional bounded summary metadata. The complete body remains untrusted tool data. The prompt keeps
internal lifecycle classification but carries no trusted internal span, so provider
projection preserves the committed generic-user envelope without adding an outer
`tau_internal`. Replay preserves the committed bytes and never selects this family
from text spelling. `AgentTree` rebuilds a non-serialized exact-node index only from
committed submitted or steered prompt facts tagged
`internal_kind=background_tool_completion`; deferred inference and open-tool-round
placement carry that fact to the eventual node. A compaction replacement never gains
this authority, while a retained suffix node keeps it. Provider projection preserves
the authoritative node's exact bytes and uses this registry family's close escaping
to prevent every other generic text item from forming a complete
`tau_background_result` envelope.

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
families. Detection accounts for the AGENTS initialization block inserted after
transcript assembly.
