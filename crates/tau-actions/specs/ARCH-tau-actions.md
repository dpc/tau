# ARCH-tau-actions: tau-actions architecture

`tau-actions` defines Tau's extension-published UI action schema and the shared
parser for slash-style action invocations.

## Responsibility

- Define serializable action schema types used by extensions to publish dynamic
  slash actions.
- Validate schemas before CLI/core consumers accept them.
- Parse a whitespace-tokenized slash line into an action id, positional argv, and
  typed named arguments.
- Build simple usage strings for validation and UI errors.

## Boundaries and dependency direction

This crate is intentionally small and dependency-light. It depends only on
`serde` plus the standard library so it can be used by extension crates, CLI
state, and core routing without pulling in harness or UI dependencies.

Consumers are expected to validate schemas before storing or presenting them.
`parse_line` also validates the supplied schema so harness-side action routing
does not trust client-provided parsed payloads without checking the raw line
against the provider schema.

## Validation contract

Action schema validation is the acceptance boundary for extension-controlled UI
and prompt-facing action metadata.

- Root command names are `/` plus an ASCII command token.
- Child command names and argument names are ASCII command tokens without `/`.
- Command tokens start with ASCII alphanumeric and then contain only ASCII
  alphanumeric, `_`, or `-`.
- Action ids and static choice values must be non-empty, contain no whitespace,
  and fit the shared token byte limit.
- Command nodes, per-action arguments, per-list choices, total arguments, total
  choices, descriptions, and aggregate schema text all have explicit budgets.

These budgets are owned here because both CLI completion state and core routing
consume the same schemas.

## Parser contract

The parser intentionally uses Tau's simple whitespace-token convention. It does
not implement shell quoting or escaping. `RestString` may appear only as the
final argument and joins the remaining tokens with single spaces.

## Sensitive argument gap

The current schema does not mark arguments as sensitive. Extensions that implement
actions accepting secrets or private authorization material must avoid echoing
those values in action output, notices, logs, and validation messages until the
schema grows a first-class sensitive-argument mechanism.
