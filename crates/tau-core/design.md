# Design decisions

This file records major design decisions currently embodied by this directory's
code, and how authoritative each decision is. It is not an architecture overview,
ADR log, todo list, roadmap, implementation guide, or changelog.

## Tool argument validation diagnostics are bounded prompt surface

Status: unconfirmed

`tool_registry.rs` validates model-produced tool arguments against Tau's
supported JSON Schema subset before a provider receives a call. Validation
errors are model-visible, so diagnostics must be actionable, deterministic, and
bounded. Prefer reporting the exact schema path, expected shape, actual value
class, and small allowed/missing/unknown field sets over returning generic
provider-style schema errors.

Extension-provided schemas and model-provided values must not determine
unbounded diagnostic size or suggestion work. Lists should be capped, long values
and path segments truncated, and near-name suggestions should use the shared
tie-safe helper from `tau-proto`.

Testing strategy: keep tool-registry validation tests in
`src/tool_registry/tests.rs`. Cover each model-visible diagnostic class and add
regressions for bounds whenever a new diagnostic includes schema-provided,
filesystem-provided, or model-provided strings.

## Tool examples are registration-validated repair metadata

Status: unconfirmed

Tool providers may attach compact examples to `ToolSpec`, including optional
declarative subcommand selectors. These examples are not part of provider-visible
tool definitions. They are registration metadata used only after a failed call,
so good calls pay no prompt-token overhead.

The registry validates examples before accepting a registration: ids and text are
bounded, selector paths must match the example arguments exactly, and function
tool example arguments must satisfy the same schema validator used for model
calls. Bad examples reject the registration clearly instead of being kept as
latent prompt-surface failures.

Testing strategy: cover schema rejection, selector path/value rejection,
compactness budgets, deterministic generic/subcommand fallback, bounded rendering,
and allowed-value diagnostics for missing or invalid selectors.
