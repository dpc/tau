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
