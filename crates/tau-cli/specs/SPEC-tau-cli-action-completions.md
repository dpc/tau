# SPEC-tau-cli-action-completions: Action completion rendering

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
