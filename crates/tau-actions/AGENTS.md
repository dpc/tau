## Crate role

`tau-actions` owns the shared v1 schema for extension-published slash actions
and the parser that turns a whitespace-tokenized slash line into a typed action
invocation.

## Review/change guidance

- Keep this crate dependency-light and reusable by CLI, core, harness, and
  extensions.
- Treat action schemas as extension-controlled prompt/UI surface: validation
  diagnostics, descriptions, completions, and parsed arguments must stay
  deterministic and bounded.
- Update `ARCHITECTURE.md` when changing schema validation budgets,
  tokenization, parser semantics, or action invocation shape.
