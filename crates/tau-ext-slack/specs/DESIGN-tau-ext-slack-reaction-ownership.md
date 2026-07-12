# DESIGN-tau-ext-slack-reaction-ownership: Reactions require remembered bridge post ownership

Status: unconfirmed

Reaction events are not general Slack-channel ingress. The bridge remembers a
bounded set of message identities returned by successful `slack_send` calls and
routes a policy-permitted verified human's add/remove reaction only to the registered agent
that created that exact post in an authorized conversation. This state is
runtime-only; reactions to unknown or evicted posts fail closed.
The authoritative thread root comes from the authenticated outbound request.
Omitted thread metadata in the Slack post response or reaction is tolerated,
while conflicting metadata prevents ownership caching or reaction routing.
