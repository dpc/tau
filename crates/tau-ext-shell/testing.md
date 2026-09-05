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

The internal `replace` / provider-visible `edit` tests cover strict object
shape, alias registration, snapshot-wide exact matching, all-or-nothing
failure, BOM and mixed-line-ending preservation, local inserted line endings,
compact results, no-op diff suppression, and the ordinary durable structured
diff for changed UTF-8 files. Harness lifecycle coverage preserves visible
`edit` requests and terminals while ext-shell receives and reports `replace`.

`apply_patch` tests retain path-labelled UI-only diffs for every changed file,
including single-file updates, add/modify/delete multi-file patches, moves,
escaped paths, and partial failures. CLI rendering tests require exactly one
path header per changed file before its hunks.
Complete-terminal frame tests measure the exact transient emit envelope at the
shared 8 MiB boundary, including producer output after local-to-wire tool-name
scoping. They also require oversized typed images to become byte-free local
errors and require singular edit/replace and path-labelled apply-patch UI diffs
to become explicit truncation markers while success, partial-failure details,
display facts, and changed paths remain intact. A focused duplicated-path-label
case requires the rewritten final frame itself to fit.

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
Allowlist coverage distinguishes absent from empty configuration, validates
strict paired absolute-workdir/raw-command glob or regex rules, exercises glob
separator/multiline semantics and regex implicit absolute anchors, and rejects
cross-rule mixing. It locks the `jj (?:log|show [a-z]{6,32})` regex at the 5/6/32/33
boundaries plus bad characters, whitespace, newlines, options, arguments, and shell
operators. Configuration coverage rejects invalid, both, neither, case-insensitive,
and resource-bounded matchers. Integration tests cover generic `shell`, ChatGPT
`shell_command`, and both user context modes, including typed-pair disclosure and
proof that denial does not spawn. Description coverage locks the 1,024-byte UTF-8
boundary, exact omission, JSON/control/Unicode and prompt-brace escaping, complete-entry
prompt de-duplication, authored denial ordering, prompt republication, and generated
model/user denial privacy. VCR coverage requires authorization before replay.

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
unconditional graceful shutdown, and startup cleanup that removes only an old
dead Tau-owned artifact while retaining old live-owner and unrelated temporary
directories. Remaining manual verification should exercise the explicit
saved-output-unavailable fallback and caller-local grep/find/edit-recovery
rendering.
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
running-work joins, and cancellation after search work starts. Deterministic
handoff barriers also cancel after scheduler dequeue and after automatic-lock
acquisition but before effect start; both regressions require no mutation and
exactly one cancelled terminal.
Large-argument ownership regressions use 1–8 MiB shell and edit calls to require
one queued payload allocation, argument-free error/workdir correlation, bounded
lock-wait snapshots, and unchanged local-to-wire terminal names.

## Discovery

AGENTS.md tests cover ancestor and local ordering, size caps, user roots before
project roots, and trusted symlink following. Skill tests cover project/user and
XDG/legacy precedence, collisions through `tau-skills`, supported symlink forms,
and canonical-directory cycle detection.
Protocol coverage feeds deterministic complete session and correlated per-agent
skill/AGENTS.md snapshots and asserts `persist=false` metadata plus
snapshot-before-context-before-readiness ordering. It also covers rapid
multi-agent initialization, session-only collision diagnostics, and checked
discovery-output failure escaping dispatch so connection teardown can release
waiters.

## Mandatory output lifecycle

Production-runner regressions force writer flush failures for sole model-tool
terminals, user-shell completion, and the live workdir setter metadata
prerequisite. Each requires the failure to escape the manual extension loop so
harness disconnect cleanup can settle retained ownership. Focused workdir tests
require a correlated canonical echo to snapshot, but not consume, the setter
reservation; only successful checked terminal publication releases it.
Production FIFO regressions also block the real writer, observe actual exhaustion
of tau-client's 64-frame detached queue, and then cover model result, ordinary
error, directory-lock error, cancellation, normal and scheduler-rejected
include-in-context user-shell completion, and the complete workdir
prerequisite/echo/terminal transaction. Each asserts exactly one correlated
reported terminal or prerequisite. Tau-client tests independently prove checked
synchronous output remains ordered and cannot starve behind continuous optional
output.
Harness test-provider fixtures use the `echo-agent`-gated empty discovery policy;
their child-process regression poisons `HOME` with a user skill and verifies the
fixture does not discover it. Production extension runners always use environment
discovery.

## `apply_patch` recovery and shell accounting

`apply_patch` regressions keep expected-context mismatch headers single-line,
place bounded multiline recovery in `details.output`, and combine that recovery
with truthful `partial_changes` plus UI-only diffs when a later hunk fails after
an earlier mutation.

The `shell` and model-visible `shell_command` accounting tests construct exact
rendered stdout/stderr records, including stream prefixes, line-ending/UTF-8
flags, and separators. They require `total_bytes` and saved artifacts to equal
that rendered UTF-8 form and to differ deliberately from raw process-byte
length where markers expand it.
