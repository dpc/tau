# SPEC-session-main-agent: Optional canonical session main

## Record justification

Main identity spans effective role configuration, session membership persistence,
lazy harness construction, UI attachment selection, and peer routing, so no
single implementation artifact owns the identity and demand contract.

## Identity and demand

A session may have zero or one canonical main agent. Concrete roles may set
`main_priority` to a nonnegative integer; this field never inherits from agent
or group defaults. After all overlays and enabled-role filtering, the lowest
number wins. Zero is a valid candidate; an absent or null effective value means
no candidate. An overlay replaces a role's priority rather than adding to it or
taking a minimum.
A tied winning minimum is a configuration error naming the roles. Each duplicate
nonwinning priority group produces one startup configuration warning without
rejecting the configuration. A session with no candidates remains valid and mainless.
The selected role remains reusable for ordinary workers, which do not gain main
identity. Runtime unavailability of the winning role fails explicitly without
falling back to another priority. Priorities select a role only for an unbound
session; they do not change an existing canonical binding.

The harness creates main on UI or peer demand through prompt-free ordinary
agent construction. Creating a session or designating a role must not
start inference. Creation and session binding are separate durable facts; a crash
may leave an unused unbound agent, but the harness must never infer main identity
from that agent's existence. Concurrent live requests share the pending candidate.

`SessionAgentLoaded.main` binds the canonical ID in session membership. That
binding survives restart and configuration changes. Ordinary unload cannot
remove main, and the harness must not replace an unavailable main with a same-role
agent. Old mainless sessions remain valid and resumable. Configuration with no
main continues ordinary explicit-role and fallback work.

## Attachment and startup

Normal create/attach UI demand defaults to canonical main when available.
Selection is attachment-local: explicit selection or creation after that demand
wins over delayed replay or initialization. If bound main is unavailable, the UI
shows an explicit unavailable/overview state rather than selecting an unrelated
worker.
The replay boundary includes canonical main identity, independently of which
agents are eligible for selection.

Explicit `--role` starts ordinary work separately; bootstrap and explicit-role
stdin retain their existing semantics. Main designation does not become a
global ordinary-role default. `agents.default_role` and the former
`inter_session.receiver` selection keys are not accepted.

## Peer input

Bare-session and exact-main input address canonical main. Reviewed exact-worker
input uses the same identity after authenticating original exact-worker
authority. Fail-closed availability, immutable release, and retained receipt
semantics are specified by
[SPEC-agent-message-delivery](SPEC-agent-message-delivery.md) and
[SPEC-tau-harness-peer-routing](../crates/tau-harness/specs/SPEC-tau-harness-peer-routing.md).
