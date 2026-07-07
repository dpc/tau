# Design decisions

This file records major design decisions currently embodied by this crate's
code, and how authoritative each decision is. It is not an architecture
overview, ADR log, todo list, roadmap, implementation guide, or changelog.

## Protocol DTO changes require nearby wire-contract tests

Status: unconfirmed

`tau-proto` owns shared wire data transfer objects. Changes to serde tags,
field defaults, validated identifiers, compatibility decoding, and CBOR codec
behavior must include tests close to the DTO definitions in this crate.

Event-name changes have an additional synchronization requirement:
`Event` serde `rename` values, `EventName` constants, and `Event::name()` are
one protocol contract. Tests should construct representative first-party
events and assert that serialized event tags, parsed `EventName` values, and
`Event::name()` stay aligned. Tests should also make transient/default
durability expectations explicit for events where that affects routing,
replay, or UI behavior.

## Provider response stats are provider-owned; turn stats are a compatibility projection

Status: unconfirmed

Provider response throughput is sampled by the provider, because the provider
owns backend request dispatch and reads the upstream response stream. Private
`provider.response_updated.response_stats` carries content-free previous/current
prompt-local samples; `previous` is the last provider sample that was actually
emitted and `current` is the new cumulative sample. The public
`agent.turn_stats_updated` event remains harness-owned as a validator/adapter and
compatibility projection, but when provider response samples are present it must
preserve provider byte/elapsed semantics rather than reconstruct throughput from
provider chunk cadence.

Live stats must not count raw wire framing, prompts, tool execution
output/results, or UI rendering text, and they must not be folded into
transcripts.

`provider.response_updated.semantic_output.non_visible_output_bytes` is the
private provider-to-harness input for non-visible generated output such as
streamed tool/custom-tool input. It is content-free, cumulative for the current
provider prompt rather than a per-update delta, stripped by the harness before
subscriber delivery, excluded from durable/public outputs, and surfaced publicly
only through harness-owned `agent.turn_stats_updated`.

Provider response/progress updates must be rate-limited to at most once per
second per prompt, except for a terminal flush immediately before the provider
prompt closes. Byte changes must not bypass this cadence. `semantic_output` and
`response_stats` are cumulative private provider-to-harness inputs sampled for
progress display, not per-chunk event streams. Providers own prompt-local
response byte counting because they read the upstream stream; the harness
validates prompt ownership, strips these private fields, and maps accepted
samples to public harness-owned turn events for current UI compatibility.
