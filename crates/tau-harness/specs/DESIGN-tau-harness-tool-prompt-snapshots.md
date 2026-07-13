# DESIGN-tau-harness-tool-prompt-snapshots: Harness-owned tool prompt-surface policy uses prompt snapshots

Status: unconfirmed

Extensions and providers publish neutral metadata (`ToolTag` and `ModelTag`) but
must not decide which model receives which tool surface. The harness evaluates
configured `tool_policy.rules` and role overrides when building the provider
prompt's effective tool list. Built-in model-specific behavior, such as the
ChatGPT shell surface, must be represented as ordinary policy data so user config
can disable or replace it by keyed rule name.

Provider tool-call authorization is against the tool snapshot advertised with the
owning prompt. Mid-turn role/model switches or later staged tool registrations
may affect future prompts, but they must not expand or shrink the set of tools
accepted for an already-dispatched prompt.

The registry may contain policy-exclusive tools with the same model-visible
alias. Snapshot construction rejects an effective surface where two enabled
tools expose the same visible name, rather than selecting by registry order.

Model-visible rejection diagnostics for prompt-owned calls follow the same
authority boundary. If a rejected call is tied to an `AgentPromptId`, unavailable
or near-name diagnostic text must derive from that prompt's tool snapshot rather
than the current role/model surface.

Tool examples attached to registrations are deliberately excluded from rendered
provider tool definitions. The harness may append one bounded example to a
model-visible failure for the owning agent branch, then remembers that example so
retry loops do not receive repeated scaffold text.

Harness tests should assert both sides of that contract: examples are omitted from
rendered provider tool definitions for good calls, and failure-triggered injection
is one-shot per agent branch while invalid registrations produce mandatory
diagnostics.
