# SPEC-tau-ext-slack-message-mutations: Slack message mutation ownership

A bounded runtime index is installed only after successful local create
publication and binds native message tuple to original agent, fact, verified
sender, conversation, and immutable thread root. A consistent `message_changed`
publishes immutable `message.edited`; unknown, evicted, or conflicting edits fail
closed and never become replacement delivery. Deletion revokes authority. Replay
does not reconstruct runtime mutation authority.

Incoming verified-human reaction add/remove routes only for an exact remembered
bridge-owned post, to its registered owning agent under current exact
conversation/root receive policy. Unknown, evicted, pre-restart, wrong-owner, or
wrong-route events fail closed. The authenticated outbound request root is
authoritative; omitted later thread metadata is tolerated, conflicting metadata
rejects. Actor display never affects authority. Every published reaction retains
Tau reply authority even if presentation omits a marker. Delete revokes and
replay does not reconstruct.
