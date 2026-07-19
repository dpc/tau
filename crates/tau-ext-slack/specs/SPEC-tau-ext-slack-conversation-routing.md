# SPEC-tau-ext-slack-conversation-routing: Slack conversation routing

One bounded exact configured conversation list uses stable aliases for exact
conversation kind/ID and optional immutable thread root. Receive and proactive
send grants are independent. Dynamic DMs are separately bounded exact-user,
source-reply-only authority. Duplicate aliases/routes and parent/child receive
overlap reject atomically. Parent receive covers threads; fixed routes normalize
to their configured root. Static receive DM wins over dynamic linking;
proactive-only static DM may coexist with a dynamic link.

Reply authority is installed only after successful local immutable message-fact
publication and requires its exact fact ID. Publication failure and replay
install nothing. Proactive send requires a current configured alias with
`proactive_send:true`, independently of receive, registration, and dynamic DMs.
Native IDs, prompt correlation, text, arrival order, thread coordinates, and
discovery results are never selectors. A call supplies exactly one `reply_to` or
`destination`; current config and exact route/install/agent/session/lifecycle
are revalidated and frozen. Without a current worker observation, read-only
`auth.test` binds bot/team before reservation.

Threads are immutable roots subordinate to one conversation. Incoming replies
use authenticated `thread_ts`; fixed routing root-normalizes the root create and
preserves the actual optional root for parent routes. Reply, prepared send,
`message.sent`, edit, and reaction routing retain the same frozen root. Callers
cannot supply or mutate thread coordinates.

Conversation discovery is disabled by default, separately tagged, config-only,
and performs no Slack I/O, startup, registration, preflight, or config freeze.
Routes are returned in alias order. Pages default to 20 and max at 32; opaque
cursors are at most 128 bytes and remain valid only while the last alias exists;
encoded results are at most 24 KiB. The exact envelope is
`{"conversations":[record...],"next_cursor"?:string}`. Each record contains
only:

```text
{
  "alias": string,
  "kind": "channel" | "mpim" | "dm",
  "scope": "conversation" | "fixed_thread",
  "description"?: string,
  "policy": {
    "receive": "mentions_only" | "all_messages" | null,
    "proactive_send": boolean
  }
}
```

Description and final-page cursor are omitted when absent. Results exclude
native IDs/roots, dynamic links, identities, registrations, selections, reply
routes, runtime health, Slack metadata, and tool/caller authorization claims.
Alias/cursor data grants no authority; send always re-resolves current config.

Legacy `channel_ids`, `listening_scope`, and `send_destinations` configuration
keys are hard errors, not compatibility aliases.

These contracts refine
[DECISION-tau-ext-slack-conversation-policy](DECISION-tau-ext-slack-conversation-policy.md).
