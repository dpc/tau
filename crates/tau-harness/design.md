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

Testing is split by owner. `tau-config` tests cover file and CLI alias
normalization, keyed rule layering, and tag-pattern parsing/rejection.
`tau-harness` tests cover evaluator ordering, role broad-to-specific overrides,
the built-in policy through the shared evaluator, and prompt-owned snapshot
authorization.

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
