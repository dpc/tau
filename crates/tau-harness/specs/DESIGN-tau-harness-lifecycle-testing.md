# DESIGN-tau-harness-lifecycle-testing: Harness lifecycle tests cover state and replay contracts

Status: unconfirmed

Message-fact tests cover extension publisher stamping, persist-before-deliver
ordering, post-commit prompt projection, invalid/unavailable targets, and
replay-without-wake behavior. Transport duplicate filtering, native routing, and
reply authority are not harness contracts. The broader crate suite covers normal
disconnect and lifecycle sequencing.

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
