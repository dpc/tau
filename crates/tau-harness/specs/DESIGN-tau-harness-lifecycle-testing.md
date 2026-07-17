# DESIGN-tau-harness-lifecycle-testing: Harness lifecycle tests cover state and replay contracts

Status: unconfirmed

Transport-ingress tests cover independent append-per-request behavior,
record-before-delivery ordering, current-capability-bound reply routing, and
correlated acceptance. Duplicate filtering is not a harness contract. The
broader crate suite covers normal disconnect and lifecycle sequencing.

Harness lifecycle/startup changes should prefer focused unit or lifecycle tests
that exercise the state machine directly, then rely on broader crate tests and
`selfci` for regression coverage. Tests for startup, disconnect, and optional
extension behavior should assert both the immediate state transition and the
replay/delivery contract for mandatory diagnostics: initial publication is not
enough if late UI subscribers must understand what happened during startup.

For optional-extension startup work, cover required/default compatibility and
each optional failure path being changed, such as config/secret/spawn failure,
pre-Ready disconnect or timeout, and `ConfigError` handling. Avoid slow wall-clock
timeout tests when a private helper can drive the same branch deterministically.
