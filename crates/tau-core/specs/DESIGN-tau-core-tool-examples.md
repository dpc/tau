# DESIGN-tau-core-tool-examples: Tool examples are registration-validated repair metadata

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
