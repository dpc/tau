---
name: tau-self-knowledge-ext-github
description: Use when configuring or troubleshooting the separately maintained Tau GitHub bridge, including receiver designation, repository admission, or migration.
advertise: false
---

# Tau GitHub bridge self-knowledge

`tau-ext-github` is a separately maintained inbound MessageBridge executable. Its
project owns the exact secret, repository, actor-admission, and polling schema.
Use a current Tau build with the automatic-receiver fix and compatible
protocol-9/SDK-0.7 extension binaries before enabling its automatic-receive
revision.

Its logical receive tool is `github_register {}`, meaning “make the authenticated
calling agent the receiving agent.” The old `{"enabled":true}` spelling remains
compatibility-only. `{"enabled":false}` and unregister forms are rejected with a
migration error, not treated as a pause. The configured extension instance scopes
the deployed tool name, and an optional `role` constrains eligible callers by
immutable creation role.

Merge this receiver-policy fragment into the full extension configuration:

```yaml
register_on_start: true # register extension automatically
role: coordinator # role to deliver to; optional, constrains registration and automatic delivery
```

`register_on_start` defaults to `false`: a saved eligible manual designation
resumes after restart, but there is no initial automatic selection or creation.
With `true`, the bridge keeps an eligible saved receiver or selects the oldest
eligible existing agent. Lazy creation requires admitted repository activity and an
explicit `role`; without a role, automatic mode selects existing agents only.
Unknown roles fail visibly. Busy receivers remain sticky, and same-agent
registration is a no-op.

Persistent sessions store one versioned concrete receiver designation for the
configured instance; ephemeral sessions retain it only in memory. An absent
snapshot may accept one unambiguous historical registration as migration evidence,
but a saved designation is authoritative. An unloaded saved agent is never
force-loaded.

An actual handoff may give the prior receiver one best-effort notice while it is
loaded. Already reported input, canonical ACKs, and the GitHub read checkpoint keep
their original target. Handoffs do not transfer old source or write authority.
GitHub actor admission remains separate from receiver-role eligibility and does not
authorize execution of repository content.
