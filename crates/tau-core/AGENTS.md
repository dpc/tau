# tau-core notes for agents

- Read `design.md` before changing tool registration, routing, validation, or
  model-visible diagnostics.
- Keep protocol-adjacent validation behavior deterministic and bounded.
- Put focused `tool_registry` tests in `src/tool_registry/tests.rs`; every test
  should state what regression or invariant it protects.
- Shared generic helpers used by multiple crates should live in a lower-level
  shared crate such as `tau-proto`, not be copied between `tau-core` and
  extensions.
