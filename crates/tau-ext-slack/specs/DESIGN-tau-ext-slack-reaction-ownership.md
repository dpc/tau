# DESIGN-tau-ext-slack-reaction-ownership: Reactions require remembered bridge post ownership

Status: confirmed, 2026-07-14, dpc

Reaction events are not general conversation ingress. The bridge remembers a
bounded set of exact identities returned by committed `slack_send` calls and
routes an eligible verified human's add/remove only to the registered owning
agent when current receive policy covers the exact conversation and root.
Unknown, evicted, pre-restart, mismatched-owner, and mismatched-route posts fail
closed.

The authoritative root comes from the authenticated outbound request. Omitted
later response/reaction thread metadata is tolerated; conflicting metadata
prevents ownership or routing. Every accepted reaction gets opaque reply
authority even though reaction presentation omits a redundant reply marker.
