Before changing this crate, discover and read the applicable Linked Specs in `specs/` and every wider `specs/` scope, then follow relevant links. Use the `linked-specs` skill when updating them and `linked-specs-review` when reviewing.

# tau-core notes for agents

- Read the applicable Linked Specs under `specs/` before changing tool registration, routing, validation, or
  model-visible diagnostics.
- Keep protocol-adjacent validation behavior deterministic and bounded.
- Put focused `tool_registry` tests in `src/tool_registry/tests.rs`; every test
  should state what regression or invariant it protects.
- Shared generic helpers used by multiple crates should live in a lower-level
  shared crate such as `tau-proto`, not be copied between `tau-core` and
  extensions.
- `AgentStore` and `SessionStore` event-log or journal changes are governed by
  `../../specs/GATE-persistence-and-extension-interface-change-approval.md`;
  obtain explicit user or maintainer confirmation of the exact semantics before
  functional changes.
