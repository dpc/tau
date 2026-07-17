# DESIGN-tau-ext-slack-conversation-discovery: Bounded configured-route discovery

Status: confirmed, 2026-07-14, dpc

`slack_conversations` is a disabled-by-default, separately tagged tool in each
prefixed Slack tool group. It reads validated local configuration only and returns
all static routes in alias order, in pages of 20 by default and at most 32. Its
opaque continuation cursor is at most 128 bytes and accepted only while its last
alias still exists. Every serialized result is at most 24 KiB. Discovery does not
contact Slack, start or preflight the worker, register an agent, grant authority,
or freeze configuration.

Each result contains only the model-facing alias, configured
`channel`/`mpim`/`dm` kind, `conversation`/`fixed_thread` scope, optional bounded
operator-authored description, and factual configured `receive` and
`proactive_send` policy. Native conversation ids and thread roots, dynamic links,
users/workspaces, registrations, selections, reply routes, runtime health, and
Slack-fetched metadata are excluded. Receive and proactive policy are not claims
about the caller's effective role, registration, selection, capability, or
connectivity.

The exact structured result envelope is
`{"conversations":[record...],"next_cursor"?:string}`. Each record is
`{"alias":string,"kind":"channel"|"mpim"|"dm","scope":"conversation"|"fixed_thread","description"?:string,"policy":{"receive":"mentions_only"|"all_messages"|null,"proactive_send":boolean}}`.
`description` is omitted when unconfigured, `receive` is null when receive is
disabled, and `next_cursor` is omitted on the final page.

Discovery is informational, not a bearer capability. `slack_send` accepts a compact
plain alias and resolves it against current configuration under the existing
extension-scoped tool authorization plus exact-route, lifecycle, and freeze
checks. It intentionally has no configuration snapshot token:
same-alias reuse is operator responsibility. Tau-issued reply selectors remain independent.
On-demand discovery trades one bounded tool call for removing O(route count) schema
tokens from every model turn.
