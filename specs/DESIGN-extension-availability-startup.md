# DESIGN-extension-availability-startup: Extension availability startup layering

Status: unconfirmed

Fresh harness startup resolves extension availability in one ordered pipeline:
configuration, supported names-only `TAU_ENABLE_EXTENSIONS`, then argv-ordered
CLI enable/disable operations. The public environment is parsed fail-closed
without logging its raw value. `TAU_EXTENSION_CLI_OVERRIDES` remains unstable
internal parent-child transport; parents clear inherited values and malformed
transport is fatal.
