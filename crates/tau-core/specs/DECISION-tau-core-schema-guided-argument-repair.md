# DECISION-tau-core-schema-guided-argument-repair: Narrow schema-guided argument repair

Authority: unconfirmed

Tau attempts argument repair only after normal schema validation fails. Repair is
intentionally narrow and non-inferential, never rewrites an already-valid call, and
must pass the ordinary validator before dispatch; ambiguous or unsuccessful repair
falls back to the normal diagnostic path.

This favors predictable tool calls over broad convenience heuristics such as string
splitting or subcommand inference. The permitted conversions are specified by
[SPEC-tau-core-tool-validation](SPEC-tau-core-tool-validation.md).
