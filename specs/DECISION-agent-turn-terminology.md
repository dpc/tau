# DECISION-agent-turn-terminology: Agent-turn and model-round terminology

Authority: confirmed, 2026-07-12, user

An **agent turn** is the outer prompt-to-final-response lifecycle. It begins when
accepted input activates an agent and remains running until the agent emits its
terminal response (or terminates) and returns control to the prompting user or
agent. Waiting for tools, processing tool results, provider retries, and repeated
model invocations all remain within the same agent turn.

A **model round** is one inner model/provider invocation. It may terminate the
agent turn or request tools. A **tool round** is the intervening execution and
collection of those tool results before another model round. Documentation and UI
state must not call an individual model round a turn where that would make the
outer lifecycle ambiguous.

An activating-input `wait` remains inside the current tool round and outer agent
turn; it does not create a suspended agent-turn state. Exact wait behavior is
specified by
[SPEC-tau-harness-activating-input-wait](../crates/tau-harness/specs/SPEC-tau-harness-activating-input-wait.md).
