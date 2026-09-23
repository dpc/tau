---
name: tau-self-knowledge-ext-discord
description: Use when configuring or troubleshooting the separately maintained Tau Discord bridge, including receiver designation, tool authority, routing, or migration.
advertise: false
---

# Tau Discord bridge self-knowledge

`tau-ext-discord` is a separately maintained MessageBridge executable. Its
project owns the exact credential, route, and transport schema. Use a current Tau
build with the automatic-receiver fix and compatible protocol-9/SDK-0.7 extension
binaries before enabling its automatic-receive revision.

In receive mode, `discord_register {}` means “make the authenticated calling agent
the receiving agent.” It has no arbitrary agent-ID argument. The tool takes empty
arguments; `{"enabled":false}` and direct `discord_unregister` invocations are
rejected rather than creating a pause control. A configured `role` restricts
eligible callers by immutable creation role.

```yaml
register_on_start: true # register extension automatically
role: coordinator # role to deliver to; optional, constrains registration and automatic delivery
```

`register_on_start` defaults to `false`. A saved eligible manual designation still
resumes after restart, but Tau neither first-selects nor creates a receiver. With
`true`, the bridge keeps an eligible saved receiver or selects the oldest eligible
existing agent. Lazy creation requires admitted external input and an explicit
`role`; without a role, automatic selection is existing-only. Busy receivers remain
sticky, and same-agent registration is a no-op.

The bridge stores a versioned receive designation in session ExtensionData under
`receive-designation.json`; it is not an operator filesystem path. Ephemeral
sessions retain that choice only in memory. The old
`restore_receive_registration` configuration key is rejected for either value with
migration guidance. A missing designation can use one unambiguous historical
registration as migration evidence, but a saved designation wins.

An actual handoff may send a best-effort notice to a still-loaded prior receiver.
Already reported input and its ACK correlation remain with their original agent.
Old source, write, and Artifact authority does not transfer. Separate send,
reaction, attachment, discovery, and send-only grants remain independent of receive
designation.
