# Design decisions

This file records major design decisions currently embodied by this directory's
code, and how authoritative each decision is. It is not an architecture overview,
ADR log, todo list, roadmap, implementation guide, or changelog.

## Harness lifecycle tests cover state and replay contracts

Status: unconfirmed

Harness lifecycle/startup changes should prefer focused unit or lifecycle tests
that exercise the state machine directly, then rely on broader crate tests and
`selfci` for regression coverage. Tests for startup, disconnect, and optional
extension behavior should assert both the immediate state transition and the
replay/delivery contract for mandatory diagnostics: initial publication is not
enough if late UI subscribers must understand what happened during startup.

For optional-extension startup work, cover required/default compatibility and
each optional failure path being changed, such as config/secret/spawn failure,
pre-Ready disconnect or timeout, and `ConfigError` handling. Avoid slow wall-clock
timeout tests when a private helper can drive the same branch deterministically.

## Harness-owned tool prompt-surface policy uses prompt snapshots

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

Schema-guided argument repair runs only in the pre-dispatch validation failure
branch. The harness executes a repaired call only after the repaired arguments
pass the same schema validator, emits a non-mandatory notice/log trace for the
local repair, and otherwise preserves the rejection/error/example behavior used
for unrepaired failures.

Testing is split by owner. `tau-config` tests cover file and CLI alias
normalization, keyed rule layering, and tag-pattern parsing/rejection.
`tau-harness` tests cover evaluator ordering, role broad-to-specific overrides,
the built-in policy through the shared evaluator, and prompt-owned snapshot
authorization.

## Runtime loop guard

Status: unconfirmed

The runtime loop guard is intentionally conservative and per-agent/branch only.
It tracks a bounded recent signature window and bounded breaker bookkeeping,
injects at most one internal pivot prompt for an obvious repeated cycle, and then
stops automatic continuation with a mandatory notice if the same cycle persists
after the breaker was dispatched.

Provider `repetition_detected` responses are treated as a loop-guard trigger with
a fixed harness-authored reason. The provider error is display-only; it is not
used as trusted pivot text.

New non-internal user input resets the guard even when the prompt is queued, and
successful foreground or background tool results reset it as clear progress.
Progress resets clear detector/breaker history and stale queued pivots but keep
unresolved in-flight tool-call argument signatures, so a successful sibling tool
in a multi-tool turn cannot make later failures argument-insensitive. Non-linear
branch/head moves invalidate the whole branch-local guard, including in-flight
signatures, and remove queued loop-guard pivots. Tests should exercise the
production response/tool/prompt wiring, not only private detection helpers: text
loops, repeated identical failures, different failure streaks, A/B/A/B suffixes,
queued user-input reset, success reset, branch invalidation, bounded breaker
state, argument-sensitive tool failures, and same-batch tool failures that must
receive the breaker before blocking.

## Ephemeral sessions suppress only session-owned persistence

Status: unconfirmed

`tau --ephemeral` is a harness/session launch mode, not an agent privacy mode.
It keeps the live session state machine, interception, prompt dispatch, and
agent stores working normally, but session-owned persistence is runtime-only for
that harness process: session membership logs, session metadata/locks,
`events.jsonl`, per-session stderr logs, and session-scoped extension data are
not written. `harness.session_dir` uses status `ephemeral` and a display-only
`<ephemeral>` path so UIs do not advertise a usable session directory.

Agent transcripts remain durable by default, including sub-agents started by
`agent_start`. Per-agent ephemerality is a separate creation policy staged from
the TUI with `/new` then `/ephemeral on`; it keeps that agent's semantic
transcript, metadata, and session membership in memory until daemon exit.
Children of ephemeral parents inherit ephemerality. Provider state, credentials,
policy/config files, runtime sockets, user/cache extension data, durable
recipients/parents, and tool side effects keep their normal persistence.

## System prompts are assembled only through templates

Status: confirmed, 2026-06-17, dpc

System prompts must be assembled through the prompt templating system. Any new
dynamic system-prompt value must be an explicit template variable/input, not text
formatted, prepended, appended, replaced, or otherwise edited around rendered
prompt content.

Ad-hoc string surgery for prompt variables such as `agent_id` is forbidden both
before and after template rendering. Exceptions are only for clearly documented
transport concerns that are not system-prompt content. This keeps custom
templates in control of placement and wording for dynamic values.
