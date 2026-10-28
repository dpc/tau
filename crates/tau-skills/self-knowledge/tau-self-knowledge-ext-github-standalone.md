---
name: tau-self-knowledge-ext-github-standalone
description: Use when configuring or troubleshooting the separately maintained standalone GitHub bridge for a dedicated agent/bot-owned account, including receiver designation, repository admission, or migration; distinguish it from tau-ext-github-watch, which reuses the user's account and setup.
advertise: false
---

# Tau standalone GitHub bridge self-knowledge

`tau-ext-github-standalone` (formerly `tau-ext-github`) is a separately maintained
inbound MessageBridge executable for a dedicated agent/bot-owned GitHub account.
`tau-ext-github-watch` reuses the user's GitHub account and setup,
with each calling agent watching selected issues and pull requests. The receiver
and repository-admission configuration below belongs to the standalone bridge,
not the watch extension.

Consult the standalone project's `README.md` and `docs/upstream-contracts.md` for
its exact secret, repository, actor-admission, and polling schema, build
instructions, compatibility evidence, and limits. Use the watch project's
`README.md` for its separate configuration. Rebuild each extension against the
SDK required by the target harness and check the owning project's compatibility
guidance. Do not assume historical compatibility evidence applies to a newer
harness.

When migrating from `tau-ext-github`, change the executable path and named Nix
package to `tau-ext-github-standalone`. Keep the configured Tau extension instance
name, tool prefix, secrets, and session data unchanged: the executable rename does
not require a receiver-state migration. See the standalone project's README for
the complete rename instructions.

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
