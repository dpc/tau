# DECISION-per-agent-extension-workdirs: Per-agent extension workdirs

Authority: confirmed, 2026-07-15, dpc

Each configured shell extension instance owns an independent durable workdir for
each agent. Its stable instance name selects the metadata namespace; tool
prefixes and transient process identities do not. Without stored metadata, the
extension's frozen validated process cwd is the default.

Setters complete only at metadata commit, and each admitted filesystem or shell
operation snapshots the last committed path. Unusable stored paths fail closed.
Ambiguous multi-instance user-shell broadcast is rejected rather than selecting
or copying state across instances.

Existing inheritable metadata provides replay and direct-child inheritance
without shared filesystem authority. Commit linearization prevents path drift;
the tradeoff is that dependent calls require a later turn and stale paths do not
silently fall back. Exact behavior is specified by
[SPEC-per-agent-extension-workdirs](SPEC-per-agent-extension-workdirs.md).
