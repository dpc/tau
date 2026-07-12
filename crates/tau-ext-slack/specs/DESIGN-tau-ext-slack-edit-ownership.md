# DESIGN-tau-ext-slack-edit-ownership: Edits require remembered committed ingress ownership

Status: inferred

Slack `message_changed` is not fresh text ingress. A bounded runtime index of
commit-confirmed `(channel, ts)` identities binds the mutation to its original
agent, canonical id, sender, conversation, and thread. Consistent edits append
immutable typed operations; unknown, evicted, or conflicting edits fail closed
without a replacement create.
