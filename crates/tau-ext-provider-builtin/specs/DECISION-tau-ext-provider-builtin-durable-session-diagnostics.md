# DECISION-tau-ext-provider-builtin-durable-session-diagnostics: Durable-session diagnostics

Authority: unconfirmed

Provider captures are enabled only by explicit current durable-session state and an
already-existing durable session directory. Providers neither infer durability from
filesystem shape nor create session roots. This fail-closed boundary avoids writing
full prompts, tools, and output for ephemeral runs that reuse an older session id.
