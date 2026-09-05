# SPEC-tau-cli-new-agent-staging: New-agent staging

## Record justification

This record spans input routing, renderer selection, prompt theming, and create-result recovery because no one local artifact can own the cross-thread client-local creation contract.

`:new` enters a local "next prompt creates an agent" mode. `:new <role>` stages
that role for the first prompt-created agent. Later no-agent role selection
commands, including `:role <role>` and role cycling, supersede the staged role;
it is not a hidden durable role authority.

The all-agent overview never creates an agent. A UI attachment whose complete
replay and current-runtime indexes are both empty enters creation implicitly;
any agent- or session-level replay failure fails closed to overview. All other
creation requires `:new` or `:agent new`. Once its first prompt is submitted,
that attachment retains one correlated pending request and a repeated Enter
sends no second request.
Requester-directed results may select or restore the submitted text only while
the request's exact local intent epoch still owns creation. Navigation or newer
editing wins over a delayed result. Draft, request, and selection state remain
process-local and are neither published nor persisted.

Options such as `:model <provider>/<model>` and `:ephemeral [on|off]` stage
one-shot properties for that next `ui.create_agent`; they are consumed by the
first prompt that creates the agent and cleared when the UI switches to an
existing agent. Bare `:new` clears only a stale staged role,
while preserving staged model and ephemeral options. Bare `:ephemeral` toggles
the staged memory-only flag, while `:ephemeral on` and `:ephemeral off` set it
explicitly. These commands do not convert existing agents in place.
