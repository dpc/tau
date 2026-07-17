# DESIGN-extension-availability-startup: Extension availability startup layering

Status: unconfirmed

Functional changes to this harness-extension lifecycle contract require the
prior standalone design and approval mandated by
[DESIGN-persistence-and-extension-interface-change-approval](DESIGN-persistence-and-extension-interface-change-approval.md).

Fresh harness startup resolves extension availability in one ordered pipeline:
configuration, supported names-only `TAU_ENABLE_EXTENSIONS`, then argv-ordered
CLI enable/disable operations. The public environment is parsed fail-closed
without logging its raw value. `TAU_EXTENSION_CLI_OVERRIDES` remains unstable
internal parent-child transport; parents clear inherited values and malformed
transport is fatal.

Deterministic embedded and daemon acceptance may explicitly bypass all ambient
startup environment/CLI compatibility transports and require an exact resolved
extension-name allowlist before spawn. The daemon retains that environment-free
policy for runtime settings reloads. This test-only policy is described by
[DESIGN-tau-e2e-deterministic-provider](../crates/tau-e2e-tests/specs/DESIGN-tau-e2e-deterministic-provider.md);
normal interactive and default daemon startup retain the ordered pipeline above.
