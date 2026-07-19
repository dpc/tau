Before changing this crate, discover and read the applicable Linked Specs in `specs/` and every wider `specs/` scope, then follow relevant links. Use the `linked-specs` skill when updating them and `linked-specs-review` when reviewing.

## Crate role

`tau-actions` owns the shared schema for extension-published slash actions
and the parser that turns a whitespace-tokenized slash line into a typed action
invocation.

## Review/change guidance

- Keep this crate dependency-light and reusable by CLI, core, harness, and
  extensions.
- Treat action schemas as extension-controlled prompt/UI surface: validation
  diagnostics, descriptions, completions, and parsed arguments must stay
  deterministic and bounded.
- Update `specs/ARCH-tau-actions.md` when changing schema validation budgets,
  tokenization, parser semantics, or action invocation shape.
- Follow [`testing.md`](testing.md) for parser/validation test ownership.
