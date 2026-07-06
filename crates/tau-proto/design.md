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

## Provider progress metadata is transient and self-contained

Status: unconfirmed

`provider.response_updated.progress` describes provider-generated semantic output
byte progress for the in-flight response: assistant text, reasoning text, and
tool/custom-tool input bytes. It must use content-free sample-window counters
that include aggregate totals over all counted items so consumers can display
bytes/rates from one update without keeping prior samples. It must not count raw
wire framing or tool execution output/results.
