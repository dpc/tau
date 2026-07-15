# tau-ext-slack security and reliability notes

Private reply, edit, and reaction state is installed only from a protocol-v11
Committed+Active result whose first canonical extension instance, target, native
occurrence identity, stable human, conversation kind/id/thread, assurance, and
policy exactly match the current pending occurrence. Inactive, rejected,
orphaned, or mismatched results install nothing.

- `std-slack` is disabled by default. Enabling its tools is a role-policy decision; configuration is not a per-agent ACL.
- App/bot tokens and Socket Mode URLs are secrets. Never log them or message bodies; diagnostics stay bounded and token/URL-redacted. Production endpoints require HTTPS/WSS.
- Slack events, metadata, edits, reactions, and text are untrusted external ingress. ACK is transport behavior, not authorization. Text stays `UntrustedExternal` and may contain prompt injection.
- `auth.test` must establish one exact bot U/W plus installing T workspace pair at startup/reconnect and before a proactive send that has no live worker observation. Supported Events API wrappers must match that installation via exact `context_team_id` or one unambiguous authorization record; missing, mixed, malformed, or conflicting evidence is dropped before `users.info` or any local/ingress effect. Top-level event team data is not authority, and a Slack Connect actor's home team may differ.
- Mutable configuration replacement discards preflight installation evidence. Reconnect must exactly match the established bot/workspace pair, compared immediately after `auth.test` and before acquiring a socket ticket. A changed, incomplete, or malformed observation disables and process-lifetime latches capability, clears pending capability correlation, retires prior pending ingress, dynamic links/selections, reply/edit routes, post ownership, and reaction authority, marks the worker offline, emits one bounded categorical restart notice, terminates without retry, and requires restart instead of admitting the replacement installation. Delayed or later capability acceptance cannot reactivate it; old sends cannot retry or install private authority.
- The exact U/W id remains sender authority and model-visible audit identity. Bounded `profile.display_name` is mutable untrusted UI-only presentation. An optional one-to-one operator `sender_aliases` binding is scoped by the harness-authenticated extension instance and has no admission/routing/reply/reaction/mention effect. Durable duplicates retain the first committed display/alias snapshot.
- Any exact installation-bot mention outside complete backtick code ranges sets the generic durable mention fact. Exactly one eligible leading occurrence is removed for routing/command compatibility; remaining eligible occurrences normalize to the semantic `@slack_bridge` token. The fact and registration's advisory reference disclose no bot/workspace id, grant no capability or authority, and do not expand egress; duplicate compatibility preserves the first normalized text/fact snapshot.
- Receive requires an exact configured kind/conversation/thread route or exact bounded dynamic D-to-U/W link plus live-human verification. Strict admits allowlisted humans; lax widens only static prompt ingress. Linking/control remain allowlist-only.
- Receive grants completion-gated opaque source replies only. Proactive send is a separate current-alias capability and grants no receive, linking, control, edit, inbound-human-reaction routing, or source-reply authority. Native ids/roots are never model-selected.
- `slack_conversations` is a separate role-authorized inventory surface. It reveals all static aliases plus operator descriptions and configured kind/scope/receive/proactive policy, including receive-only routes; it never reveals native ids/roots, dynamic links, identities, runtime state, or Slack metadata. Use exact tool policy or separate prefixed instances to isolate inventories.
- Fixed threads use their configured root for creates, local replies, edits, reactions, and sends. Parent receive covers children; overlapping parent/child receive is rejected.
- Harness capability/session/tool/agent checks and extension route checks are both required. Configuration freezes before an authorized post or reaction API attempt, or after successful worker preflight.
- Runtime caches, routes, ownership, selections, and links are bounded. Committed creates are durably deduplicated/restorable by native conversation+timestamp when Slack retries; edits require restored create ownership and inbound human reactions require same-process post ownership. Crashes/API ambiguity do not provide exactly-once delivery.
- `slack_send` owns a 1,024-entry non-evicting session/process ledger. It
  reserves before I/O and admits at most 64 active delivery workers. Initial
  HTTP and the sole event-driven retry remain off the protocol reader; each
  attempt has a 30-second HTTP timeout, responses are capped at 64 KiB, and the
  retry must begin within the 60-second logical-call horizon. Exact
  lifecycle/config/route authority is revalidated before each attempt and
  completion; disconnect/EOF retires authority before workers are woken.
  Completion output is written and flushed through an acknowledged background
  path; writer failure retires outbound authority, wakes workers, and requests
  shutdown. Replay output shares the 64-worker cap, coalesces per call id, and
  waits in a bounded FIFO when saturated; agent unload retains correlation but
  cannot restore private reaction authority.
  Awaiting-Tau, durably completed, definitive, cumulative
  ambiguity/copy range, and cancellation states are retained. Full capacity
  rejects before freeze/I/O. One
  initial-plus-one byte-identical retry is deliberately at-least-once: an
  ambiguous first attempt can leave one or two Slack copies; two ambiguous
  attempts can leave zero, one, or two. Restart clears this
  boundary; there is no durable outbox, `client_msg_id`, reconciliation, or
  exactly-once guarantee. Already-started synchronous HTTP is process-owned and
  may outlive the protocol `run` return for at most its 30-second request timeout;
  retired workers cannot retry or restore local authority.
- Slack HTTP/identity/post diagnostics are closed typed categories. Raw provider
  bodies/codes/headers/errors, tokens, native ids, mention text, and message text
  do not enter displays, model errors, notices, or logs. Agent mrkdwn rejects raw
  native controls; bridge-owned reflected output is escaped, disables mrkdwn and
  link expansion, and is component/final bounded.
- Agent-authored text cannot contain raw `<@`, `<!`, or `<#` Slack controls.
  `mention_source_user` defaults false and can generate only the exact verified
  human bound to a live `reply_to`; it is invalid for configured destinations
  and for the bot/Slackbot. The generated mention is frozen with the exact
  byte-identical retry body. Protocol-v11 `ToolStarted` is the scoped-tool lease
  for one logical call, and the harness revalidates tool authority on completion.
- Identity/API outages fail closed. Slack, workspace administrators, members, and Slack Connect participants may read transported text; this is not end-to-end encrypted.
- Supported ingress uses one persistent serial in-memory FIFO bounded at 64 queued/in-flight occurrences. Capacity is reserved before ACK, retained through terminal processing, and released on ACK failure or terminal rejection/application. Saturation, actor failure, and harness-writer closure stop later ACK admission. Reconnect preserves accepted order; session/config/process teardown invalidates late authority. Process death after ACK can still lose work.
- Latency observability is TRACE-only and non-durable. It permits process-local volume/order correlation but never native IDs, payloads, tokens, URLs, response bodies, agent IDs, or stable hashes. Keep retention bounded and never promote occurrence/request ordinals to metric labels.
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
