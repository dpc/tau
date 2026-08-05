# GATE-extension-filesystem-mediation: Mediate extension filesystem access

## Gate

Tau extensions may access an execution host's filesystem directly only when that
access is intrinsic to the extension's explicit user-facing function on that
host. All other extension filesystem operations—including Tau or extension
state, configuration, credentials, caches, checkpoints, diagnostics, captures,
and spooled output—must cross the harness-extension interface and be performed
by the harness. Ordinary filesystem reads required to load and run the extension
process and its runtime dependencies are exempt.

## Justification

The user wants functional filesystem effects to occur where the selected
extension executes, including on a remote host, while keeping Tau's operational
data under harness authority. Harness mediation ensures operational data lands
on the correct host and remains subject to harness placement, isolation, and
persistence policy.
