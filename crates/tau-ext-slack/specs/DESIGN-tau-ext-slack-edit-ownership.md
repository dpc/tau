# DESIGN-tau-ext-slack-edit-ownership: Edits require remembered published ownership

Status: inferred

Slack `message_changed` is not a new base message. A bounded runtime index of
successfully written `(channel, ts)` identities binds the edit fact to its
original agent, message-fact ID, sender, conversation, and thread. Consistent
edits publish immutable `message.edited` facts; unknown, evicted, or conflicting
edits fail closed without a replacement delivery fact.
