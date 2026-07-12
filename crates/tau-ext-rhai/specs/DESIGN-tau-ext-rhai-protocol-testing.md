# DESIGN-tau-ext-rhai-protocol-testing: Protocol tests drive `run`

Status: unconfirmed

Behavior tests for this crate should prefer serialized Tau protocol frames sent
through `run` and assertions on outbound frames. Shell behavior should be tested
through Rhai tools returning `ShellJob` when possible, so tests cover script API
admission, deferred tool result/error emission, tau-client startup staging, and
shell process supervision together.
