# DESIGN-tau-ext-slack-conversation-policy: Unified exact conversation policy

Status: confirmed, 2026-07-14, dpc

Slack receive and initiation policy uses one bounded `conversations` list. Each
record binds a stable alias to one exact native conversation and optional fixed
thread, declares its explicit `channel`, `mpim`, or `dm` kind, and independently
enables `receive` and `proactive_send`. Dynamic direct-message discovery is a
separate explicit, bounded, exact-user-bound, receive-and-source-reply-only policy.

This replaces the asymmetric global `channel_ids`, `listening_scope`, and
`send_destinations` concepts. Those keys are errors rather than compatibility
aliases. One list makes duplicate aliases/routes and receive parent/child overlap
rejectable atomically, keeps receive mode local to the route it affects, and
lets one record combine both permissions without granting either implicitly.

Aliases, not native ids, are the only proactive selectors. Receive creates
source-bound opaque reply authority but no proactive authority; proactive send
creates no receive, reply, linking, or control authority. Parent receive includes
all threads, while a fixed-thread receive route isolates state and normalizes its
root create. Static receive DM policy takes precedence over dynamic discovery;
proactive-only static DM policy remains compatible with a dynamic reply link.

The tradeoff is an intentionally breaking migration and more explicit records.
It avoids preserving conceptual legacy whose global behavior cannot accurately
represent channel/private/MPIM/DM/thread policy.
