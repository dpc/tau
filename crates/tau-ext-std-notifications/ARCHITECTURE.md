# tau-ext-std-notifications architecture

`std-notifications` is a user-facing side-effect bridge. It listens to harness events and emits terminal-facing notification actions; it does not own agent execution.

The extension runs on `tau-client`'s manual-loop runtime. Tau-client owns the
startup prelude, exact event subscriptions, `Ready`, raw configuration dispatch,
live-only event dispatch, and outbound writer thread. This crate owns the policy
loop on top: it waits for either harness input or the next idle/summary deadline,
and after clean input EOF it switches to a timer-only path so pending idle hooks
can still fire.

## Configuration keys

Hook and option keys use snake_case (`agent_start`, `agent_end`, `agent_idle`, `agent_idle_all`). Unknown fields must stay rejected so typoed notification config surfaces as a harness config error.

## Trigger boundaries

Prompt-start and turn-end notifications are user-visible main-turn effects. They use `PromptOriginator::User` prompt/provider events and ignore extension side conversations so delegate work and idle-summary queries do not ring sounds or perturb per-agent idle timers.

Visible-turn notification state is tracked per agent, not globally. Duplicate
prompt suppression, the last prompt text used by templates, deferred final
responses, and active background-tool blockers belong to the agent whose turn
created them. This lets multiple loaded agents make interleaved progress without
one agent suppressing another agent's `agent_start`/`agent_end` hooks or mixing
`turn.*` template data.

Provider prompt-start events are scoped through the known `agent_prompt_id` to
agent mapping. If that mapping is missing, the event is ignored for per-agent
idle cancellation instead of clearing all idle timers, because a global fallback
would let one active agent prompt suppress another agent's pending idle
notification.

`agent.prompt_terminated` marks the corresponding user-originated prompt id
consumed, clears that prompt's in-flight notification state, and emits no
completion hooks or idle timers. It does not by itself clear active
background-tool blockers; those remain indexed by tool call until
`tool.background_result` / `tool.background_error` so terminal tool events can
clean state without ringing a terminated prompt's completion.

Background blockers are learned from provider-visible background placeholders
(`provider.tool_result`, and `tool.result` for compatibility, with
`kind = background_placeholder`) using the owner learned from the preceding
tool-call `provider.response_finished`; active blockers are removed only by
terminal background result/error events.

`agent_idle_all` uses harness-owned `agent.state` snapshots as its busy/idle source of truth, together with `session.agent_loaded` and `session.agent_unloaded` membership. Provider final-response events only update template context (`turn.user_prompt` / `turn.agent_response`) for the eventual all-idle notification; they do not decide whether the session is idle. This keeps side-query prompt/response traffic from clearing a pending all-idle notification.

## Idle timers

`agent_idle` timers are per completed user turn. `agent_idle_all` timers are keyed by session and are armed when a tracked session transitions from at least one running loaded agent to no running loaded agents. A visible `agent.state = running` clears only pending all-idle timers for sessions containing that running agent. Summary side agents spawned by this extension are correlated through `agent.start_accepted` for pending `idle-*` query ids and ignored for all-idle membership/busy tracking until the matching `agent.start_result`, so they cannot cancel the notification they are producing. `ui.prompt_draft` extends idle timers that have not yet started summary side queries. Draft events are consumed only as liveness signals here; `target_agent_id` and draft text are ignored for notification policy.

`session.shutdown` drops all-idle timers and all-idle membership/busy tracking
for the closing session. This prevents delayed EOF idle draining or later state
events for a reused agent id from emitting notifications for a session the
harness has already left. If the dropped timer was already waiting for a summary
side-agent result, only that timer's query id is removed from summary-agent
ignore tracking; unrelated in-flight summary side agents remain ignored until
their own result arrives.

Idle summary side-agent requests include a bounded copy of the captured user
prompt and assistant response in the instruction. Do not assume the side
conversation has inherited the transcript that triggered the notification.

## Testing strategy

Unit tests drive the extension through encoded harness frames and assert emitted
events. State-machine changes should add or update tests for event ordering,
replay filtering, side-agent originator filtering, per-agent interleaving,
prompt termination, background-tool deferral, all-idle session membership,
config reloads, and idle deadline timing. Keep
timer windows short and bounded; use `UnixStream::pair` tests only when the test
must observe an emitted request before sending the matching response. Terminal
side-effect changes need regression coverage for config-time validation and
runtime template-rendered data. Runtime migration changes should preserve the
manual-loop contracts: exact live subscriptions, post-EOF idle draining, explicit
Disconnect without idle draining, and fatal surfacing of protocol decode/read
errors from tau-client rather than silently treating corrupted input as EOF.
