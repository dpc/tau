# SPEC-tau-cli-action-completions: Action completion rendering

## Record justification

Dynamic action dispatch, asynchronous completion rendering, and transcript
lifecycle ownership span the CLI input, event-rendering, and transcript-state
paths, so no single implementation artifact can coherently own the contract.

Action schema ownership, publication, and lifecycle follow
[SPEC-action-declarations-and-outcomes](../../../specs/SPEC-action-declarations-and-outcomes.md).
CLI parsing and command completion use the same currently selected root binding:
built-in commands take precedence, and competing dynamic roots select the lowest
logical owner by configured extension name and then numeric logical instance id.
Schema snapshot replacement or withdrawal and owner removal recompute that
selection from the schemas that remain published.

Before sending a dynamic `action.invoke`, the CLI records its invocation id and
the currently viewed agent or no-agent transcript. The first matching
`action.result` or `action.error` consumes that owner and renders in that
transcript, even if another transcript is visible when it arrives.

A completion whose invocation id is unknown, already consumed, replayed after
the ownership map was cleared, or otherwise lacks a recorded owner follows
ordinary event rendering in the currently visible transcript. Session reset
clears all recorded owners. Initial no-agent adoption retargets still-pending
owners only as specified by
[SPEC-tau-cli-transcript-context](SPEC-tau-cli-transcript-context.md).
