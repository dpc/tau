# DESIGN-tau-cli-action-completion-snapshots: Dynamic action completion snapshot ownership

Status: unconfirmed

Dynamic extension action completions (`action.result` and `action.error`) render
in the transcript snapshot that was viewed when the CLI sent the matching
`action.invoke`, not whichever agent is selected when the completion arrives.
The CLI records `ActionInvocationId -> viewed agent/no-agent snapshot` before
sending the invoke because completion events carry an invocation id but no agent
id. Unknown or replayed completions keep the existing visible fallback.
