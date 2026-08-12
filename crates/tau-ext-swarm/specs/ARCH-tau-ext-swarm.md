# ARCH-tau-ext-swarm: Tau Swarm extension architecture

`tau-ext-swarm` is a normal configured first-party extension. `SwarmRuntime`
owns Tau protocol folding and canonical prompt loopback, `SessionProjection`
owns one coherent bounded current-session view, `SwarmApplication` implements
the published Tau Swarm client contract, and an owned worker runs the pinned
Iroh client without blocking Tau's protocol reader.

The extension waits for `session.replay_complete` before publishing. An agent
whose Tau display name is absent retains that absence while replay and live
events are folded. At the unchanged Tau Swarm v4 publication boundary, the
extension encodes that absence as an empty `name` string; a later explicit Tau
display-name fact replaces it with the nonempty name.

A session switch cancels and joins the previous worker and clears session-local
blocker history, updates, and acknowledgements while retaining the
process-incarnation command table. Ordinary Iroh
reconnects retain that process-memory state and restart publication from a
coherent snapshot when retained changes no longer cover the reader revision.
The owned worker generation also owns authoritative publication health. Normal
return or panic unwind makes that health indeterminate before any optional
warning; panic-abort builds terminate the extension process. The mutating
`blocker` and `update` tools serialize their complete mutation against
retirement, then reject without changing local state until a fresh session
replay starts a live publisher.
`SwarmRuntime` generates one collision-resistant application-incarnation ID at
process startup and retains it across session workers and ordinary reconnects.
A replacement process declares a fresh ID, allowing Tau Swarm to fence ambiguous
old commands and supersede the previous process's active lifecycle state.

Remote prompt and blocker-answer acceptance requires the matching canonical
Tau `agent.prompt_submitted` or `agent.prompt_steered` event carrying the exact
command ID as `ctx_id`. Missing loopback is indeterminate and never authorizes
a duplicate Tau submission within the same session incarnation.
This uses the existing
[SPEC-internal-prompt-submit-requests](../../../specs/SPEC-internal-prompt-submit-requests.md)
interface rather than defining another prompt protocol.

The extension registers `blocker` and `update` in the `swarm` tool group
with default model exposure disabled. Starting or connecting the extension does
not grant those tools to a role; role `enable_tool_groups: [swarm]` or exact
`enable_tools` configuration opts in through the ordinary tool policy order.
An instance `tool_prefix` structurally qualifies all of these policy names, as
defined by [SPEC-extension-tool-prefixes](../../../specs/SPEC-extension-tool-prefixes.md).
