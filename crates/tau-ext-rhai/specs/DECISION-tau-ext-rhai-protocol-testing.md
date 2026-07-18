# DECISION-tau-ext-rhai-protocol-testing: Exercise integrations through `run`

Authority: unconfirmed

Behavior tests drive serialized Tau protocol frames through the public `run`
boundary and assert outbound frames. Shell behavior is exercised through registered
Rhai tools returning `ShellJob` where possible.

This deliberately covers script API admission, deferred results, tau-client startup
staging, and shell supervision together instead of testing only isolated helpers.
