# DECISION-tau-cli-action-completion-snapshot-ownership: Action completion snapshot ownership

Authority: unconfirmed

An asynchronous extension action completion belongs to the transcript viewed when
the matching action was invoked, keyed by invocation id, rather than to whichever
agent is selected when the completion arrives. This compensates for completion
events carrying an invocation id but no agent id and prevents navigation races from
moving action output between conversations. Exact fallback behavior is specified
by [SPEC-tau-cli-action-completions](SPEC-tau-cli-action-completions.md).
