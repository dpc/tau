# SPEC-passive-extension-context: Passive attributed extension output

## Record justification

Admission, interception, journal folding, replay deduplication, and
extension-visible acknowledgement span protocol, harness, core, and SDK code;
no single component owns the complete append contract.

Configured extensions may submit `extension.passive_context_append_request` to
append output to a loaded agent without activating inference. This is a generic
transcript facility: the harness does not interpret commands, exit status, UI
request IDs, or the operation that produced the body.

## Admission and provenance

The request names the bound session, loaded agent, nonempty operation ID (at most
256 UTF-8 bytes), and body (at most 64 KiB). All configured extension kinds may
request it; attached UIs cannot. The admitted request follows ordinary transient
publication and interception before semantic processing. The harness validates
the captured configured generation and current target, then publishes a separate
`agent.passive_context_appended` fact through ordinary interception and semantic
append. Peers cannot publish canonical facts or result events.

The canonical fact records the authenticated configured publisher, operation ID,
target, and rendered text. The shared `extension_output` family has ordered
`publisher` and `operation_id` attributes, escaped with the shared attribute
renderer. Its exact close is `</extension_output>`; body collisions become
`&lt;/extension_output&gt;`. Committed historical `tau_extension_output` bytes remain
unchanged and recognizable. Nested tags do not establish another provenance
boundary. This is extension output, not direct human input or trusted harness
instructions. Canonical facts are immutable under interception; Drop rejects
the append. Original target privacy classification survives interception.

## Operation and outcome semantics

The operation key is `(agent, configured publisher, operation ID)`, independent
of connection lifetime and selected transcript branch. Identical rendered text
returns `Committed` without adding another fact. Different text for an accepted
key returns `Failed`. The replay-derived index survives history-cache eviction.
Concurrent duplicates serialize through the normal publication queue; no
pre-persistence reservation or execution ledger is introduced.

`extension.passive_context_append_result` is a transient, harness-authored
outcome. `Committed` means the semantic store accepted the canonical fact (or
already contained it), with the store's ordinary durability guarantees. It is
not a separate disk-flush or OS-operation guarantee. `Failed` means this attempt
did not append a new fact. Publication and persistence failure never rerun the
external operation. Missing delivery, including an intercepted request or
result, leaves an unconfirmed outcome; callers may retry **only the append**
with the same operation ID and body.

The canonical fact uses normal agent persistence, including memory-only
ephemeral agents, normal transcript ordering during inference/tool barriers,
and generic debug suppression for ephemeral targets. It creates no activation
obligation, on live append or replay. There is no atomicity between any external
side effect, an extension's transient UI result, and this transcript append.

## Verification

Focused coverage must verify publisher attribution and envelope escaping,
non-activation, duplicate/conflicting operations, cold replay, interception
drop and replacement, target loss, persistence failure, and ephemeral privacy.
