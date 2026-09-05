# GATE-new-generic-user-payload-envelopes: Envelope new generic user payloads

## Gate

Any change that introduces a new free-form textual payload kind, or changes the
model-visible framing or provenance of an existing payload kind, in Tau's shared
generic user-role transcript channel must present that payload in exactly one
top-level family registered by the shared XML-lite payload-envelope contract.
Payload text and component-local tag-shaped syntax cannot establish or redefine an
envelope family.

Protocol-typed non-message context items, including ordinary foreground,
harness-synthetic background-placeholder, and `wait` tool results, are outside this
constraint because their `ToolResultItem` kind already identifies their channel.
Other typed `ContentPart` variants, the separate system/developer prompt channel,
non-model text, metadata-only message facts with no free-form body, assistant-role
message facts, and immutable provider-native replay items are also outside it.

Existing payload projections are not required to migrate solely because this gate
is established. They must not be copied as precedent for a new payload kind. If a
later change modifies an existing kind's model-visible framing or provenance, that
change is governed by this gate.

## Justification

The user wants payloads multiplexed through Tau's generic user-role context item to
have consistent, reviewable source boundaries, while avoiding a migration project
for every existing path. A prospective shared contract prevents future agents from
inventing component-local XML-like envelopes without changing the trust, authority,
routing, persistence, activation, or tool semantics of existing payloads.
