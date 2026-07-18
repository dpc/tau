# DECISION-per-agent-extension-workdirs: Per-agent extension workdirs

Authority: confirmed, 2026-07-15, dpc

Each configured shell extension instance owns an independent durable workdir for
each agent. Its stable instance name selects the metadata namespace; tool
prefixes and transient process/connection identities do not. When no key exists,
the extension's own frozen validated process cwd is the only safe default because
instances may inhabit unrelated filesystem namespaces.

Setters complete only at their correlated metadata commit, and every admitted
filesystem or shell operation snapshots the last committed path through later
queueing and lock waits. Unusable stored paths fail closed until explicitly
repaired. Ambiguous multi-instance user-shell broadcast is rejected rather than
selecting or copying state across instances.

Using existing durable inheritable metadata provides replay and direct-child
inheritance without inventing shared filesystem authority. Commit linearization
and admission snapshots prevent concurrent work from drifting between paths; the
tradeoff is that dependent calls require a later turn and stale paths do not
silently fall back.

Exact metadata, inheritance, tool, error, snapshot, prompt, and UI behavior is
specified by
[SPEC-per-agent-extension-workdirs](SPEC-per-agent-extension-workdirs.md).
This choice implements
[REQ-independent-manipulation-extension-instances](REQ-independent-manipulation-extension-instances.md)
and preserves the configured-extension boundary in [SECURITY.md](../SECURITY.md).
