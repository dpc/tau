# SPEC-tau-ext-slack-agent-reactions: Agent-authored Slack reactions

Reaction refs are at most 128 bytes. Emoji use a 1–64-character lowercase ASCII
base of letters, digits, `_+-`, with an optional exact `::skin-tone-[2-6]` suffix.
Only add/remove operations exist; strict parsing rejects unknown fields.

Eligible targets are locally written and flushed create/edit facts and successful
sends, never reaction occurrences, help/control output, failed or unpublished
events, or arbitrary Slack objects. A successful local fact write/flush happens
before target activation; it is not a harness commit ACK. A successful send
publishes `message.sent` before its ordinary result. Unknown, stale, evicted,
cross-agent, cross-instance, or cross-route refs fail generically before I/O. The
exact item timestamp is the mutation target while its immutable authenticated
conversation/thread root remains separate routing authority.

Source-reply targets revoke on unregister; proactive targets do not. Agent
unload, session/config/process retirement clears authority; reconnect alone does
not. Ownership is reserved by native semantic tuple. `already_reacted` never
adopts ownership; `no_reaction` clears only an already local owner. Ambiguous and
other failed outcomes preserve any existing ownership but never create
ownership. Same-call replay repeats no I/O. Self-reaction echoes are ignored.

Owned refs are pinned. Only unpinned refs may be evicted. Capacity is 2,048
target refs, 1,024 ownership tuples, and 256 attempt records. In-flight unowned
adds reserve ownership capacity; exhaustion rejects before I/O. Slack HTTP is
bounded to 30 seconds. There is no sleep or automatic retry. `Retry-After` is
strictly parsed and clamped for a closed typed error; diagnostics contain no raw
body, token, native ID, or text. Config freezes immediately before the first
authorized API attempt. Writer failure activates nothing, and stable
same-process replay does not repost or rewrite; there is no crash guarantee.
