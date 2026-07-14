# tau-ext-slack security and reliability notes

- `std-slack` is disabled by default. Enabling its tools is a role-policy decision; configuration is not a per-agent ACL.
- App/bot tokens and Socket Mode URLs are secrets. Never log them or message bodies; diagnostics stay bounded and token/URL-redacted. Production endpoints require HTTPS/WSS.
- Slack events, metadata, edits, reactions, and text are untrusted external ingress. ACK is transport behavior, not authorization. Text stays `UntrustedExternal` and may contain prompt injection.
- Receive requires an exact configured kind/conversation/thread route or exact bounded dynamic D-to-U/W link plus live-human verification. Strict admits allowlisted humans; lax widens only static prompt ingress. Linking/control remain allowlist-only.
- Receive grants completion-gated opaque source replies only. Proactive send is a separate current-alias capability and grants no receive, linking, control, edit, inbound-human-reaction routing, or source-reply authority. Native ids/roots are never model-selected.
- `slack_conversations` is a separate role-authorized inventory surface. It reveals all static aliases plus operator descriptions and configured kind/scope/receive/proactive policy, including receive-only routes; it never reveals native ids/roots, dynamic links, identities, runtime state, or Slack metadata. Use exact tool policy or separate prefixed instances to isolate inventories.
- Fixed threads use their configured root for creates, local replies, edits, reactions, and sends. Parent receive covers children; overlapping parent/child receive is rejected.
- Harness capability/session/tool/agent checks and extension route checks are both required. Configuration freezes before an authorized post or reaction API attempt, or after successful worker preflight.
- Runtime caches, routes, ownership, selections, and links are bounded. Committed creates are durably deduplicated/restorable by native conversation+timestamp when Slack retries; edits require restored create ownership and inbound human reactions require same-process post ownership. Crashes/API ambiguity do not provide exactly-once delivery.
- Identity/API outages fail closed. Slack, workspace administrators, members, and Slack Connect participants may read transported text; this is not end-to-end encrypted.
- Recheck these invariants and the adversarial matrix in `specs/ARCH-tau-ext-slack.md` when changing config freeze, routing, sender admission, mutations, dedup, lifecycle, or capability schemas.

## Agent-invoked reaction boundary

`slack_react` is a separately role-gated, disabled-by-default externally visible
action (`slack:react`). It accepts only exact commit-accepted Tau-issued refs,
never native Slack IDs or aliases, and revalidates live source/config authority.
Adds establish bounded same-agent runtime ownership only after unambiguous
success; removes require that ownership. Ambiguous effects are never adopted,
and lifecycle clearing may leave remote residue rather than risk cross-agent or
cross-session removal. The optional surface requires only `reactions:write` and
does not add listing/discovery. Slack members, administrators, Slack Connect,
and workflows can observe or act on bot reactions.
