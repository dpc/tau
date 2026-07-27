# ARCH-tau-actions: tau-actions architecture

`tau-actions` defines Tau's extension-published UI action schema and the shared
parser for command-mode action invocations.

## Boundaries and dependency direction

The crate owns the serializable schema, schema validation, and conversion of a
raw command line into a typed action invocation. Extensions publish schemas,
CLI state presents and parses them, and core routing revalidates the raw line
against the provider schema. This keeps extension-controlled action metadata
behind one shared acceptance boundary rather than trusting a client-provided
parsed payload.

`tau-actions` depends only on `serde` and the standard library so extension,
CLI, core, and harness crates can share it without introducing UI or harness
dependencies.

## Parser contract

Action commands use whitespace tokens without shell quoting or escaping.
`RestString` is the final argument and joins remaining tokens with single
spaces. The schema API and its tests own token grammar, resource budgets,
argument kinds, usage rendering, and exact diagnostics.
