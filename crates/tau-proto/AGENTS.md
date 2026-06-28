# tau-proto instructions

- Read `ARCHITECTURE.md` before changing protocol DTOs, event names, message envelopes, serde/CBOR codec helpers, or validated wire identifiers.
- Read `design.md` before changing protocol verification strategy, event-name synchronization tests, compatibility fixtures, or fixture organization.
- Read the repository root `SECURITY.md` before changing event routing, custom event validation, tool-result/error payloads, or any field that can carry extension-provided data.
- Keep `docs/events.md` aligned when changing event names or selected event semantics.
