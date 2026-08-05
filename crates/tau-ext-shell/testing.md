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

`replace` tests cover strict object shape, snapshot-wide exact matching,
all-or-nothing failure, BOM and mixed-line-ending preservation, local inserted
line endings, compact results, no-op diff suppression, and the ordinary durable
structured diff for changed UTF-8 files.

`apply_patch` tests retain path-labelled UI-only diffs for every changed file,
including single-file updates, add/modify/delete multi-file patches, moves,
escaped paths, and partial failures. CLI rendering tests require exactly one
path header per changed file before its hunks.

Workdir coverage includes initialization, replay precedence, malformed state,
setter admission/commit/cancellation, concurrent rejection, and call-local
`cwd`/`workdir` behavior. Harness-boundary tests cover provider cardinality,
stale-session rejection, targeted execution, multi-UI projection, delivery loss,
bounded IDs, delayed events, and exactly one terminal result.
Prompt coverage asserts the shell-owned declaration and prose, default/prefixed
and unavailable rendering, effective-capability and contributor filtering,
shared-fragment coalescing/lifecycle, and the cold-resume prompt oracle.
Protocol tests assert that every user-shell producer emits the `_reported`
progress/completion names; harness-boundary tests separately lock report commit,
canonical mapping, exact generation/route authority, activation ordering, and
post-completion-commit transcript injection.
Schema coverage keeps `shell_command` limited to its current `workdir` spelling;
the removed GPT `cwd` spelling appears only in an explicitly named legacy
compatibility test.

## Processes, locking, and scheduling

Process tests cover Linux/Android/macOS TTY-backed output descriptors, persistent
stdin EOF/readiness, separated stdout/stderr capture, foreground exit, timeout,
cancellation, signals, bounded output, truncation, and descendants retaining PTY
user endpoints. Unix-only helpers are gated and may skip when unavailable.

Saved-output regressions cover representative native read/list/user-shell
renderings under the 10 KiB visible cap and model-shell rendering under the
15 KiB cap, the 16 MiB hard cap,
honest complete/incomplete metadata, exact-path privacy,
ordinary expiration only after both 32 later relevant calls and 15 minutes,
and unconditional graceful shutdown. Remaining manual verification should
exercise startup cleanup of locked/dead crash leftovers without
deleting live owners, the explicit saved-output-unavailable fallback, and
caller-local grep/find/edit-recovery rendering.
Model-shell VCR coverage verifies that recordings own a bounded sibling `shell-output`
artifact and replay regenerates a fresh ephemeral path with complete or
incomplete metadata rather than persisting a stale path.
Repository-owned pager fixtures cover protected environment precedence, preserved
`TERM`, protected `JJ_PAGER`, deliberately ordinary `MANPAGER` / `BAT_PAGER`,
the explicit opt-out, post-EOF pager stalls, timeout, and model/user surface
parity without relying on host pager configuration.
Other targets cover equivalent foreground and bounded-drain behavior;
Windows-only changes are compiled for Windows when practical. See
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
Protocol coverage feeds deterministic complete session and correlated per-agent
skill/AGENTS.md snapshots and asserts `persist=false` metadata plus
snapshot-before-context-before-readiness ordering.
Harness test-provider fixtures use the `echo-agent`-gated empty discovery policy;
their child-process regression poisons `HOME` with a user skill and verifies the
fixture does not discover it. Production extension runners always use environment
discovery.
