# DECISION-positive-persistence-publication-metadata: Use positive persistence publication metadata

Authority: confirmed, 2026-07-22, dpc

Generic event publication metadata uses `persist: bool`: `true` requests
ordinary eligible semantic persistence, while `false` requests live-only
publication. Every former `transient=false` value becomes `persist=true`, and
every former `transient=true` value becomes `persist=false`.

`persist` remains publication metadata rather than an unconditional storage
command. Event-family classification retains authority over semantic stores:
families excluded for either metadata value remain excluded, while
`agent.prompt_created` and the existing canonical terminal-tool exceptions
remain persisted even when their publication metadata is `persist=false`.
Interception, admission, deferred or captured envelopes, commit, routing, and
codec boundaries must preserve the positive bit without changing those
classifications.

The old field is removed completely. There are no aliases, migrations,
dual-read paths, compatibility defaults, or deprecation period under
[DECISION-no-backward-compatibility](DECISION-no-backward-compatibility.md).
This refines the publication and persistence contract in
[DECISION-generic-peer-event-emission](DECISION-generic-peer-event-emission.md)
and is approved under
[DECISION-persistence-and-extension-interface-change-approval](DECISION-persistence-and-extension-interface-change-approval.md).

## Rationale

Positive persistence metadata makes call sites and routing conditions describe
their intent directly instead of relying on the double negative
“not transient.” Preserving the independent event-family classifier avoids
turning a wire-level vocabulary improvement into a replay or durability change.
