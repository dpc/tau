# tau-ext-zulip

This extension bridges untrusted Zulip traffic into Tau. Read the local Linked Specs, `SECURITY.md`, `testing.md`, and the root external-message boundary before changing transport, admission, routing, mutations, or lifecycle behavior.

Keep bot email and API-key secrets, queue IDs, native user IDs, native message IDs, stream IDs, and exact participant routes out of logs, notices, tool results, and canonical fact authority. Configuration keys use snake_case and unknown fields fail closed.

Update `tau-self-knowledge-ext-zulip` after user-visible capability or operational changes.
