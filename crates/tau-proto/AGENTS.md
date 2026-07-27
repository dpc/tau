Before changing this crate, discover and read the applicable Linked Specs in `specs/` and every wider `specs/` scope, then follow relevant links. Use the `linked-specs` skill when updating them and `linked-specs-review` when reviewing.

# tau-proto instructions

- Read `specs/ARCH-tau-proto.md` before changing protocol DTOs, event names, message facts, serde/CBOR codec helpers, or validated wire identifiers.
- Read the applicable Linked Specs under `specs/` before changing protocol verification strategy, event-name synchronization tests, compatibility fixtures, or fixture organization.
- Keep DTO wire-contract tests near their definitions. Event-name tests must keep
  serde tags, `EventName` constants, `Event::name()`, and applicable default
  durability aligned.
- Read the applicable trust-boundary records in the repository-root `specs/` directory before changing event routing, custom event validation, tool-result/error payloads, or any field that can carry extension-provided data.
- Keep `docs/events.md` aligned when changing event names or selected event semantics.
- Event-persistence and harness-extension protocol changes are governed by
  `../../specs/GATE-persistence-and-extension-interface-change-approval.md`;
  obtain explicit user or maintainer confirmation of the exact semantics before
  functional changes.
