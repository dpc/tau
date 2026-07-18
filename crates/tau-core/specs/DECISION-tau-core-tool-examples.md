# DECISION-tau-core-tool-examples: Failure-only tool examples

Authority: unconfirmed

Tool examples are validated registration metadata used only after a failed call,
not provider-visible tool definitions on successful calls. Invalid examples reject
registration rather than becoming latent prompt-surface failures.

This keeps good-call prompt definitions compact while still providing bounded,
provider-independent repair guidance. Validation and selection behavior is specified
by [SPEC-tau-core-tool-validation](SPEC-tau-core-tool-validation.md).
