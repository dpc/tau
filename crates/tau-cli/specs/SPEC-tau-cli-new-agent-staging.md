# SPEC-tau-cli-new-agent-staging: New-agent staging

`:new` enters a local "next prompt creates an agent" mode. `:new <role>` stages
that role for the first prompt-created agent. Later no-agent role selection
commands, including `:role <role>` and role cycling, supersede the staged role;
it is not a hidden durable role authority.

Options such as `:model <provider>/<model>` and `:ephemeral [on|off]` stage
one-shot properties for that next `ui.create_agent`; they are consumed by the
first prompt that creates the agent and cleared when the UI switches to an
existing agent. Bare `:new` clears only a stale staged role,
while preserving staged model and ephemeral options. Bare `:ephemeral` toggles
the staged memory-only flag, while `:ephemeral on` and `:ephemeral off` set it
explicitly. These commands do not convert existing agents in place.
