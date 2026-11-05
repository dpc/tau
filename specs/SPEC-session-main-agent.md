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

The harness creates main on UI, peer, or explicit serve-bootstrap demand through
prompt-free ordinary agent construction. Creating a session or designating a role
must not start inference. Creation and session binding are separate durable facts; a crash
may leave an unused unbound agent, but the harness must never infer main identity
from that agent's existence. Concurrent live requests share the pending candidate.

`SessionAgentLoaded.main` binds the canonical ID in session membership. That
binding survives restart and configuration changes. Ordinary unload cannot
remove main, and the harness must not replace an unavailable main with a same-role
agent. Old mainless sessions remain valid and resumable. Configuration with no
main continues ordinary explicit-role and fallback work.

`tool_policy.canonical_main_only` restricts exact final registered tool names to
the committed, available main. The harness applies this final veto when
materializing model-visible tools and dispatching authenticated calls, including
calls from old prompt snapshots. Role and model grants cannot override it;
parentlessness, a main role, bootstrap metadata, and a pending main candidate
confer no authority. Role-level `required_tools`
validation checks provisioning; prompt validation checks the final instance tool
surface and rejects requirements for withheld tools rather than granting them.
Ordinary same-role construction remains supported.

## Attachment and startup

Normal create/attach UI demand defaults to canonical main when available.
Selection is attachment-local: explicit selection or creation after that demand
wins over delayed replay or initialization. If bound main is unavailable, the UI
shows an explicit unavailable/overview state rather than selecting an unrelated
worker.
The replay boundary includes canonical main identity, independently of which
agents are eligible for selection.

An explicit startup role starts ordinary work separately. Without one, the harness
selects the effective main role for ordinary startup and developer previews.
Mainless configurations retain ordered role fallback.

Serve bootstrap targets canonical main when one is bound or configured, sharing
the same lazy resolver as UI and peer demand. A conflicting explicit startup role
fails rather than creating an ordinary sibling. Without main, bootstrap retains
ordinary creation. The harness skips previously admitted bootstrap IDs before
reading the source or validating the role; it never promotes or migrates existing
ordinary bootstrap agents. The user explicitly approved this routing and
deduplication contract for ticketwfcs, replacing the former ordinary-bootstrap
contract.

Canonical-main bootstrap commits a non-inheritable, harness-reserved admission
marker before the separate visible interaction and prompt transitions. An
admitted ID never runs again on restart, even after a stop between marker and prompt.
This is at-most-once admission, not guaranteed or exactly-once model execution.
Creation without an admission marker does not suppress a later bootstrap attempt.
`agents.default_role` and the former
`inter_session.receiver` selection keys are not accepted.

## Peer input

Bare-session and exact-main input address canonical main. Reviewed exact-worker
input uses the same identity after authenticating original exact-worker
authority. Fail-closed availability, immutable release, and retained receipt
semantics are specified by
[SPEC-agent-message-delivery](SPEC-agent-message-delivery.md) and
[SPEC-tau-harness-peer-routing](../crates/tau-harness/specs/SPEC-tau-harness-peer-routing.md).
