# Design decisions

This file records major design decisions currently embodied by this directory's code, and how authoritative each decision is. It is not an architecture overview, ADR log, todo list, roadmap, implementation guide, or changelog.

## Agent-turn and model-round terminology

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

## Extension availability startup layering

Status: unconfirmed

Fresh harness startup resolves extension availability in one ordered pipeline:
configuration, supported names-only `TAU_ENABLE_EXTENSIONS`, then argv-ordered
CLI enable/disable operations. The public environment is parsed fail-closed
without logging its raw value. `TAU_EXTENSION_CLI_OVERRIDES` remains unstable
internal parent-child transport; parents clear inherited values and malformed
transport is fatal.

## Successful tool-result displays use `ok`

Status: confirmed, 2026-07-09, user

Successful tool-result display metadata uses the standard short `ok` status
consistently. Tool-specific success synonyms must not replace `ok` when they add no
distinct lifecycle information. A different status is appropriate only when it
represents a documented non-success lifecycle state.

## AGENTS.md and skill discovery follow symlinks

Status: confirmed, 2026-07-03, user

Tau intentionally follows symlinks while discovering and loading trusted prompt
inputs: AGENTS.md files, AGENTS.*.md files, skill roots, skill directories, and
Markdown skill files. This supports normal dotfile, shared-repository, and
project-local layouts where instruction files or skill collections are linked
from another location.

This is an accepted prompt trust-boundary decision, not a sandbox escape. These
files are already trusted instructions that can steer the agent once loaded, so
refusing symlinks would mainly break legitimate layouts without creating a
meaningful security boundary. Implementations must still bound traversal and
reads, track canonical directories for skill traversal so symlink cycles cannot
recurse forever, and document that users should only run Tau in repositories and
skill roots whose prompt content they trust.

## Event subscribers list concrete events by default

Status: confirmed, 2026-07-03, user

Tau protocol subscriptions should use exact event-name selectors for the events
the subscriber actually handles. Whole-category prefix subscriptions such as
`agent.*`, `tool.*`, or `provider.*` are reserved for cases where the subscriber
is intentionally a generic observer for that category and the broader traffic is
part of its design.

This keeps new event types from automatically expanding existing subscribers'
traffic, replay catch-up, prompt-surface exposure, or side-effect triggers.
First-party extensions and UIs that only react to a known subset of events should
therefore spell that subset out explicitly and update it deliberately when their
handlers learn a new event.

## Cargo-crap gates are lowered by refactoring, not exceptions

Status: confirmed, 2026-06-23, user

Cargo-crap limits are project quality gates. Increasing configured limits,
whether in `.cargo-crap.toml` or in the Nix cargo-crap gate definitions, is not
an acceptable way to make failures pass.

Regenerating or editing `nix/cargo-crap-baseline.json` to add exceptions to the
cargo-crap limits is also not acceptable. The baseline is an opt-out from
refactoring that exists only to keep already-known historical debt from blocking
unrelated work, and it should shrink over time. New or worsening cargo-crap
failures should be addressed by simplifying/decomposing code and/or adding
meaningful tests.

The active Nix absolute gate remains at CRAP 500 while existing historical
offenders above CRAP 100 are being worked down. Once those offenders are below
CRAP 100, the intended next threshold is CRAP 100; that lowering should happen
by refactoring the offenders, not by adding new exceptions.

Tau keeps shared cargo-crap defaults in the repository-root `.cargo-crap.toml`
so local developer runs and Nix CI use the same non-CI-specific policy values.
Nix cargo-crap derivations should pass only run-specific inputs and per-job
overrides that differ from the shared defaults, such as workspace selection,
LCOV path, baseline path, min/top cutoffs, output/format, fail mode, and
intentionally different report/regression thresholds. The filtered Nix source
must include `.cargo-crap.toml` so config changes invalidate cargo-crap CI
outputs. Do not put CI-only LCOV/baseline paths or allowlist exceptions in the
shared config.
