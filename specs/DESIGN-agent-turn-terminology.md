# DESIGN-agent-turn-terminology: Agent-turn and model-round terminology

Status: confirmed, 2026-07-12, user

An **agent turn** is the outer prompt-to-final-response lifecycle. It begins when
an accepted input activates an agent and remains running until that agent emits
its terminal response (or termination) and returns control to the prompting
user or agent. Waiting for tools, processing tool results, provider retries, and
repeated model invocations are all inside the same agent turn.

A **model round** is one inner model/provider invocation within that agent turn.
It can produce a terminal response or request tools. A **tool round** is the
intervening execution and collection of those requested tool results before a
subsequent model round. Documentation and UI state must not call an individual
model round a turn when that would make the outer lifecycle ambiguous.

An activating-input `wait` remains inside the current tool round and outer agent
turn. It does not create a suspended agent-turn state: accepted input completes
the tool call and is folded into the following model round. The waiter itself is
runtime-only and is repaired, not recreated, after a cold daemon restart.
