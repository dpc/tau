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

## Agent turn stats are transient and harness-owned

Status: unconfirmed

`agent.turn_stats_updated` describes content-free live stats for an active agent
turn. The harness publishes it with current and previous cumulative samples so
consumers can calculate byte/rate deltas without depending on provider-owned
progress metadata. It must not count raw wire framing, prompts, tool execution
output/results, or UI rendering text, and it must not be folded into transcripts.

`provider.response_updated.semantic_output.non_visible_output_bytes` is the
private provider-to-harness input for non-visible generated output such as
streamed tool/custom-tool input. It is content-free, cumulative for the current
provider prompt rather than a per-update delta, stripped by the harness before
subscriber delivery, excluded from durable/public outputs, and surfaced publicly
only through harness-owned `agent.turn_stats_updated`.
