# tau-ext-std-notifications architecture

`std-notifications` is a user-facing side-effect bridge. It listens to harness events and emits terminal-facing notification actions; it does not own agent execution.

## Configuration keys

Hook and option keys use snake_case (`agent_start`, `agent_end`, `agent_idle`, `agent_idle_all`). Unknown fields must stay rejected so typoed notification config surfaces as a harness config error.

## Trigger boundaries

Prompt-start and turn-end notifications are user-visible main-turn effects. They use `PromptOriginator::User` prompt/provider events and ignore extension side conversations so delegate work and idle-summary queries do not ring sounds or perturb per-agent idle timers.

`agent_idle_all` uses harness-owned `agent.state` snapshots as its busy/idle source of truth, together with `session.agent_loaded` and `session.agent_unloaded` membership. Provider final-response events only update template context (`turn.user_prompt` / `turn.agent_response`) for the eventual all-idle notification; they do not decide whether the session is idle. This keeps side-query prompt/response traffic from clearing a pending all-idle notification.

## Idle timers

`agent_idle` timers are per completed user turn. `agent_idle_all` timers are keyed by session and are armed when a tracked session transitions from at least one running loaded agent to no running loaded agents. A visible `agent.state = running` clears only pending all-idle timers for sessions containing that running agent. Summary side agents spawned by this extension are correlated through `agent.start_accepted` for pending `idle-*` query ids and ignored for all-idle membership/busy tracking until the matching `agent.start_result`, so they cannot cancel the notification they are producing. `ui.prompt_draft` extends idle timers that have not yet started summary side queries.

Idle summary side-agent requests include a bounded copy of the captured user
prompt and assistant response in the instruction. Do not assume the side
conversation has inherited the transcript that triggered the notification.

## Testing strategy

Unit tests drive the extension through encoded harness frames and assert emitted
events. State-machine changes should add or update tests for event ordering,
replay filtering, side-agent originator filtering, background-tool deferral,
all-idle session membership, config reloads, and idle deadline timing. Keep
timer windows short and bounded; use `UnixStream::pair` tests only when the test
must observe an emitted request before sending the matching response. Terminal
side-effect changes need regression coverage for config-time validation and
runtime template-rendered data.
