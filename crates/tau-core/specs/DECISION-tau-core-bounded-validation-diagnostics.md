# DECISION-tau-core-bounded-validation-diagnostics: Bounded tool validation diagnostics

Authority: unconfirmed

Model-visible tool argument validation diagnostics are actionable and deterministic,
but extension schemas and model values cannot cause unbounded output or suggestion
work. Diagnostics use shared tie-safe suggestions rather than provider-specific or
unbounded error rendering.

This accepts truncation and capped field lists in exchange for keeping malformed
calls useful to repair without turning validation into an uncontrolled prompt or
compute surface. Exact behavior is specified by
[SPEC-tau-core-tool-validation](SPEC-tau-core-tool-validation.md).
