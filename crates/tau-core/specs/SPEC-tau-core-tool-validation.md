# SPEC-tau-core-tool-validation: Tool validation, repair, and examples

Function-tool arguments are validated against Tau's supported JSON Schema subset
before dispatch. The subset covers boolean schemas; object properties, required
fields, and `additionalProperties`; primitive and union `type`; `enum`; array
`items`; and numeric, string, and array bounds. Unknown schema keywords are
ignored rather than becoming harness errors.

## Diagnostics

Each failure reports the bounded JSON-like schema path and a class-specific
message. Type failures report expected and actual types. Enum failures report the
rejected value, a bounded allowed-value list, and a shared tie-safe nearest-name
suggestion only when unambiguous. Object failures collect bounded missing or
unknown field sets. Bound failures report the violated numeric, length, or item
limit; non-finite numbers are rejected. Diagnostics do not claim fields that do
not apply to their failure class.

A diagnostic path retains at most 200 source characters and its message retains
at most 1,024 source characters; either may append one ellipsis when text was
omitted. Schema- or value-derived lists contain at most 16 items. Each rendered
item retains at most 40 source characters before any ellipsis, quoting delimiter,
or list omission marker is added. Dynamic property path segments, rejected
values, and allowed values obey these rendering budgets.

Nearest-name suggestion work has a separate fail-closed budget: it observes at
most 128 candidates and considers requested or candidate names only when each is
at most 80 characters. Exceeding either budget produces no suggestion.

## Conservative repair

Repair is attempted only after ordinary validation fails. It may:

- parse a string as a JSON object or array only when one exact schema type requires
  that container and the JSON has no duplicate object keys;
- remove `null` only from a known optional field whose schema rejects null;
- wrap a scalar in a one-element array only when one exact schema type requires an
  array, recursively repairing the item when possible;
- parse a base-10 `i64` string consisting of ASCII digits with one optional
  leading minus, but no plus sign or whitespace, when one exact schema type
  requires an integer; or
- parse exactly lowercase `true` or `false` when one exact schema type requires a
  boolean.

It does not split strings, infer subcommands, repair union/ambiguous type schemas,
remove required or valid nulls, accept malformed or duplicate-key JSON, or rewrite
already-valid arguments. A string beginning with `[` but failing array parsing is
not scalar-wrapped. Nested object fields and array items may use the same rules.
Every returned value must pass ordinary validation before dispatch; otherwise the
original failure path is used. Repair traces contain at most 16 bounded steps plus
an omitted-step count. Their summary retains at most 1,024 source characters and
may append one ellipsis.

## Registration examples

A tool may register at most 32 examples. Example ids are nonempty, unique, and at
most 64 characters; optional titles and notes are at most 120 characters each.
Arguments are limited to 600 scalar characters and 128 CBOR nodes and, for
function tools, must pass ordinary argument validation.

A subcommand selector has one to eight nonempty map-key path segments of at most
40 characters each. The path must exist in that example's arguments and its value
must equal the declared selector value. Any violation rejects the registration
with a bounded error rather than retaining a latent invalid hint.

After a failed call, Tau selects the lexicographically-by-id first example whose
selector matches the failed arguments, otherwise the first generic example,
otherwise the lexicographically first example. A missing selector match may append
up to 16 sorted, deduplicated allowed selector values. The complete model-visible
hint retains at most 1,200 source characters and may append one ellipsis, with
rendered arguments using at most half that source-character budget.

These contracts feed the failed-call path in
[SPEC-tau-harness-prompt-dispatch](../../tau-harness/specs/SPEC-tau-harness-prompt-dispatch.md).
