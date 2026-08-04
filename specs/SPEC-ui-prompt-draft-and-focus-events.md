# SPEC-ui-prompt-draft-and-focus-events: Attached-UI liveness observations

## Record justification

Prompt-draft and focus observations span terminal UI producers, protocol DTOs,
harness authority/interception/persistence, and extension subscribers. This
record keeps their distributed live-only contract coherent.

## Authority and publication

`ui.prompt_draft` and `ui.focus_changed` retain their existing wire names. Only
an attached harness-assigned socket UI may author either event. Dedicated
external-message socket peers, non-UI socket peers, missing or disconnected
clients, and configured or unconfigured extension-path peers have no authority.
Harness-internal direct publication remains outside peer admission.

Admission performs no liveness-domain work. Accepted events retain their
caller-selected `Emit.persist` value and enter ordinary generic interception,
commit, and live broadcast with the UI's run-local source. Interceptors may drop
an observation or replace any payload field while retaining the exact event name
and source. Ordinary subscribers react only after commit.

UI publishers do not participate in extension `Ready` activation. A disconnected
socket immediately loses authority; there is no pre-Ready queue for this row.

## Producer metadata and persistence

Both event variants default to transient when published without explicit
metadata. The interactive CLI's established producer path instead uses
`HarnessInputMessage::emit`, so its actual `Emit.persist` value is true. The
harness preserves that value for interception and generic publication rather
than coercing it.

Both observations retain runtime sequencing, debug publication, and live
broadcast but never enter agent, session, or restore semantic stores for either
`persist` value. They have no cold replay, current-state snapshot, or late
historical catch-up. Changing CLI producer metadata or debug-log treatment
requires a separate approved interface or persistence change.

## Prompt draft

A draft carries the attached session, optional viewed agent, and optional full
current prompt buffer. The interactive CLI defaults to content-free drafts;
`send_prompt_draft_content: true` in layered `cli.yaml`/`cli.d` config includes
the full current buffer. `None` is omitted from the wire representation rather
than serialized as `null`. Modern producers use `Some(agent_id)` for an existing
viewed transcript and `None` for an unscoped or new-agent draft. Absence must
not be reinterpreted as the current agent.

The CLI emits its first snapshot immediately and then coalesces later edits to
at most one snapshot per second. It invalidates stale pending snapshots when
the target changes or submission advances the draft epoch. `std-notifications`
is the only first-party subscriber: it treats the event solely as typing
liveness and ignores session, target, and text while extending eligible idle
deadlines.

## Focus

A focus observation carries the attached session and whether terminal focus was
gained or lost. Tau currently has no first-party focus subscriber. The event is
available to exact live subscribers without creating harness state.

The shared decoded-message bound applies. This specification adds no
row-specific text bound, current-session validation, throttling, or hostile-IPC
policy under the trusted same-UID UI boundary.

## Scope

This specification implements only the UI liveness row of
[SPEC-peer-event-publication](SPEC-peer-event-publication.md).
It does not change terminal focus handling, the general publisher envelope, or
any state-changing or dedicated UI request row.
