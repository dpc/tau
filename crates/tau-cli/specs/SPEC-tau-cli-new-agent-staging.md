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

Options such as `:model <provider>/<model>`, `:effort <value>`, and
`:ephemeral [on|off]` stage one-shot properties for that next
`ui.create_agent`; they are consumed by the first prompt that creates the agent
and cleared when the UI switches to an existing agent. Bare `:new` clears only
a stale staged role, while preserving staged model, effort, and ephemeral
options. `:effort reset` clears the staged effort. Bare `:ephemeral` toggles the
staged memory-only flag, while `:ephemeral on` and `:ephemeral off` set it
explicitly. These commands do not convert existing agents in place.

The created agent's model and effort overrides apply to all of its prompts
while it remains loaded, including after suspend/resume. They are
loaded-runtime identity, not durable creation facts: a daemon restart or cold
reload restores the persisted role and resolves its then-current model and
effort. Each already-started prompt retains its exact persisted model
parameters as historical facts.

With an existing agent selected, `:effort <value>` updates that loaded agent's
runtime override and `:effort reset` clears it. The update affects only prompts
whose parameters are selected afterward; an in-flight prompt retains its
already-selected parameters. The per-agent override outranks the current
runtime role effort, which outranks normally resolved configuration defaults.
Provider/model capability mapping remains responsible for the effective native
effort. The command is neither completed nor accepted from the non-creating
overview.
