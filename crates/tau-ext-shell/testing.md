# Testing tau-ext-shell

This document owns the evolving test catalog for shell tools. Behavioral authority
remains in the applicable Linked Specs.

## Images, protocol, and UI

Image tests use deterministic PNG/JPEG/WebP fixtures to cover sniffing, animation
rejection, allocation and workspace budgets, geometry, EXIF orientation before
crop, and crop failures. They distinguish provider content from display metadata.
The opt-in real-provider oracle is documented in
[`read_image` visual-fidelity oracle](../../docs/read-image-fidelity-oracle.md).
Cross-crate provider tests own Responses wire shape, Lite detail omission,
fail-closed routing, request-wide raw and data-URL budgets, and digest-preserving
data-URL redaction.

Registration tests assert model-visible schemas and validate provider-owned
examples with `tau_core::validate_tool_examples`; custom/freeform examples receive
separate semantic coverage. Runtime tests lock startup subscriptions and
publications before `Ready`. UI tests assert `ToolUseState` progress and terminal
projection, including inferred directory-lock modes.

Workdir coverage includes initialization, replay precedence, malformed state,
setter admission/commit/cancellation, concurrent rejection, and call-local
`cwd`/`workdir` behavior. Harness-boundary tests cover provider cardinality,
stale-session rejection, targeted execution, multi-UI projection, delivery loss,
bounded IDs, delayed events, and exactly one terminal result.
Schema coverage keeps `shell_command` limited to its current `workdir` spelling;
the removed GPT `cwd` spelling appears only in an explicitly named legacy
compatibility test.

## Processes, locking, and scheduling

Process tests cover foreground exit, timeout, cancellation, signals, bounded output,
truncation, and descendants retaining pipes. Unix-only helpers are gated and may
skip when unavailable. Supported non-Unix targets cover equivalent foreground and
bounded-drain behavior; Windows-only changes are compiled for Windows when
practical. See
[`SPEC-tau-ext-shell-process-lifecycle`](specs/SPEC-tau-ext-shell-process-lifecycle.md).

Directory-lock tests cover manual and automatic lifecycle, ancestry conflicts,
path-local FIFO fairness, same-owner rules, cancellation, cleanup, force unlock,
backend parity, lease reaping, reconfiguration, cross-instance identity, adaptive
polling, and predicate-backed same-process wakeups. See
[`SPEC-tau-ext-shell-directory-locking`](specs/SPEC-tau-ext-shell-directory-locking.md).

Scheduler tests cover bounded admission, queued cancellation, drop, worker wakeup,
running-work joins, and cancellation after search work starts.

## Discovery

AGENTS.md tests cover ancestor and local ordering, size caps, user roots before
project roots, and trusted symlink following. Skill tests cover project/user and
XDG/legacy precedence, collisions through `tau-skills`, supported symlink forms,
and canonical-directory cycle detection.
