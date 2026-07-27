# SPEC-tau-ext-slack-message-mutations: Slack message mutation ownership

## Record justification

Mutation authority spans Socket Mode decoding, ingress identity and route checks,
runtime source/posted-message indexes, report submission, and delete cleanup, so
no single implementation area can own the complete contract coherently.

A bounded runtime index is installed only after successful local create-report
submission and binds native message tuple to original agent, report, verified
sender, conversation, and immutable thread root. A consistent `message_changed`
submits transient `message.edited_reported`; unknown, evicted, or conflicting edits fail
closed and never become replacement delivery.

Incoming verified-human reaction add/remove routes only for an exact remembered
bridge-owned post, to its registered owning agent under current exact
conversation/root receive policy. Unknown, evicted, pre-restart, wrong-owner, or
wrong-route events fail closed. The authenticated outbound request root is
authoritative; omitted later thread metadata is tolerated, conflicting metadata
rejects. Actor display never affects authority. Every submitted reaction report retains
Tau reply authority even if presentation omits a marker. Delete revokes and
canonical-fact replay does not reconstruct runtime mutation authority.
