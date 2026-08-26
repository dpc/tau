# ARCH-tau-ext-shell: tau-ext-shell architecture

`tau-ext-shell` owns Tau's local filesystem and subprocess tools. It must avoid process-global cwd changes after startup: concurrent tool workers resolve paths against per-agent state instead.

`read_image` uses the same remembered-cwd and bounded regular-file authority as
`read`. It reads one opened file once and accepts sniffed PNG, JPEG, or WebP
only. Source and normalized bytes are each capped at 8 MiB; pre-decode sides are
at most 8192 pixels and decoded area at most 16,777,216 pixels. Decoder-reported
output is capped at 64 MiB before allocation and one extension-wide permit
bounds concurrent decoded memory. WebP uses a stricter 4,194,304-pixel and
32-MiB decoded-output cap because its decoder has additional workspace
allocations. Crop and resize may temporarily hold the decoder's bounded raster
and one equally bounded transformed raster (at most 128 MiB combined); the
single extension-wide decode permit covers this whole preparation interval.
Animated inputs are rejected. EXIF orientation is applied before
an optional half-open oriented-source region crop. Metadata is stripped through
same-format re-encoding. Bare and explicit `high` output retains the
2048-pixel-side and 2,500-patch bounds; experimental `overview` uses 1024-side
and 600-patch bounds. Both are provider high-detail content because overview is
a local transform. The typed transformed image is provider/transcript data and
therefore drives request/context accounting; generic display metadata contains
safe source/oriented/region/output geometry, profile, patches, format, and byte
count without bytes.
Terminal producers measure the actual transient emit envelope against
tau-client's shared 8 MiB outbound-frame limit. A typed image whose complete
terminal envelope does not fit becomes a local byte-free tool error; it is never
converted to base64 or generic text.

## Per-agent instance workdir metadata

The extension instance name from `configure.instance_name` defines the workdir
metadata key: `ext_<instance>_cwd`. The built-in core shell instance therefore
uses `ext_core-shell_cwd`. If multiple shell instances are configured, each uses
its own instance-derived key and keeps an independent cwd map.

An absent key initializes from the validated process cwd frozen after startup
configuration. The CLI and harness do not seed a built-in instance or copy paths
between namespaces. The extension publishes transient
`agent.metadata_set_request` events; committed harness-authored
`agent.metadata_set` / `agent.metadata_unset` facts are the source of truth. The
extension updates its in-memory cache only after seeing those events, publishes
fresh `agent_context.workdir` after each committed change, and emits
`extension.context_ready` only after publishing the initial cwd context for a
loaded agent. Both publications use `persist=false` wire metadata and commit before
the harness updates prompt projection or releases readiness. Metadata values are
inheritable so child agents start in the parent's remembered workdir. Stored
stale or malformed values remain
authoritative and fail closed until an explicit absolute setter repairs them.
See
[SPEC-agent-metadata-requests-and-canonical-facts](../../../specs/SPEC-agent-metadata-requests-and-canonical-facts.md).

Every tool invocation snapshots the committed workdir at admission. Relative
resolution, command execution, and directory-lock setup retain that snapshot
through queueing and lock waits. Generic call-level `shell.cwd` and
ChatGPT-facing `shell_command.workdir` remain invocation-only overrides and
never emit metadata. The latter is distinct from the persistent top-level
`workdir(path)` read/set transaction governed by
[SPEC-per-agent-extension-workdirs](../../../specs/SPEC-per-agent-extension-workdirs.md).

## tau-client runtime boundary

The shell extension uses `tau-client` for protocol startup, exact
subscriptions, configuration error reporting, replay/live dispatch filtering,
and serialized output. Shell-specific policy remains local: cwd folding,
session/agent lifecycle cleanup, directory-lock scheduling, tool cancellation,
and `StartAgentResult` correlation are owned by `ShellRuntime`.

Worker and scheduler output goes through a shell-local `Output` adapter. Optional
progress and diagnostics use `ClientHandle::send_detached` in production, so
they do not block worker threads on protocol flush. Sole model-tool terminals,
user-shell completion, session and per-agent discovery, correlated context,
prerequisite metadata, and readiness use checked ordered writes instead. The
extension retains tool ownership until checked terminal flush and retains a
workdir-setter reservation through its canonical metadata echo until the
echo-correlated terminal flushes. A failure in mandatory output
wakes and exits the extension loop, allowing disconnect cleanup to release
harness waiters rather than leaving a connected provider with missing
settlement. Configure-time tool
re-registration deliberately uses synchronous
`register_local_tool`: tau-client buffers that declaration behind static startup
defaults and flushes the configured override before `Ready`. Tests can use an
mpsc-backed adapter for direct state-machine coverage.

Before mandatory tool-terminal output, ext-shell measures the complete encoded
report envelope. If optional structured UI diff data alone makes a successful
or partially successful file-mutation report too large, ext-shell replaces only
that diff with an explicit truncation marker. It preserves the model-visible
success or error, changed-file summary/details, display status, and path/range
arguments, so an already-applied effect remains truthfully reportable.

## User-shell reports

On Linux, Android, and macOS, model and user shell commands attach independent
pseudo-terminals to stdout and stderr. This makes both output descriptors
TTY-like without merging the captured streams. Stdin remains closed, preserving
persistent EOF/readiness for the input-less tool surface. Output PTYs disable
terminal newline translation to preserve the line-ending, byte-count, and
truncation contracts. Tau creates its PTY endpoints atomically close-on-exec;
this does not strengthen independent descriptor allocation in Rust's macOS
fork/exec implementation, whose exec-status pipe retains a small inheritance
race. The child still starts in its own session without gaining the harness
terminal as a controlling terminal. Other targets, including unvalidated BSD
targets, retain pipe capture and closed stdin.
See
[SPEC-tau-ext-shell-process-lifecycle](SPEC-tau-ext-shell-process-lifecycle.md).

The `read`, `grep`, `find`, `ls`, edit-recovery, and user `!` / `!!` surfaces
use a 10 KiB visible bound. Model `shell` / `shell_command` uses a 15 KiB
visible bound. Each preserves its native rendering and metadata. When a cap
truncates output, ext-shell
saves at most 16 MiB of the same ordered native rendering in a private
temporary artifact. Complete saved artifacts use `full_output_path`; artifacts
that hit the saved cap use `saved_output_path` plus explicit incomplete
metadata. Ordinary expiration requires both 32 later relevant ext-shell calls
and 15 minutes, with cleanup triggered by a relevant call. Graceful shutdown
remains unconditional. Startup-call cleanup independently removes provably dead
crash leftovers older than 15 minutes.
On Unix, each artifact directory is mode `0300`, each file is mode `0600`, and
an owner lock prevents crash cleanup from touching a live process's artifacts.
Platforms where ext-shell cannot enforce equivalent privacy report
`saved_output_unavailable: true` instead of publishing a path.
Model-shell VCR recordings keep the bounded saved rendering in a sibling
`<call-id>.shell-output` side artifact rather than embedding an ephemeral path
or up to 16 MiB in the size-limited YAML cassette. Replay creates a fresh
ephemeral artifact from that owned side file.

At the shared spawn boundary for both model and user shell commands, ext-shell
applies ordinary `shell.extra_env`, then normally protects `PAGER`, `GIT_PAGER`,
`GH_PAGER`, `JJ_PAGER`, and `SYSTEMD_PAGER` with `cat`. It preserves `TERM`.
The explicit `shell.non_interactive_pager: false` opt-out disables this
protection. `MANPAGER`, `BAT_PAGER`, and arbitrary application-specific pagers
remain ordinary.

An optional `shell.allowlist` provides a best-effort command/cwd guardrail for
model `shell`, ChatGPT-facing `shell_command`, and user `!`/`!!`. It is not a
sandbox or security boundary. Absence preserves unrestricted execution; an
explicit empty list denies all. Each rule conjunctively binds an absolute
canonical-workdir glob to exactly one raw submitted shell-language command matcher:
the existing `command` glob or a `command_regex` regular expression. Any complete
matching pair allows execution. Both match the whole raw command case-sensitively;
regexes receive implicit absolute anchors, including across newlines. Workdir `*`
stays within one path component while `**` crosses components; command globs retain
globset grammar and treat separators and newlines as ordinary characters.
Authorization uses bounded matcher compilation and occurs before VCR replay and
process spawn. It does not inspect the configured shell/wrapper, environment,
`PATH`, shell expansion, or resolved executables. Denials disclose each typed
command matcher with its paired workdir so an agent can choose a permitted command.
Fixed internal subprocesses such as the `rg` used by `grep` do not participate.

When the allowlist is present, the shell-owned prompt fragment also declares
that enforcement is enabled and lists the effective typed command/workdir
selector pairs. The list sorts and de-duplicates presentation entries but does
not alter authored-rule matching. It says `none (all shell commands are denied)`
for an explicit empty allowlist. Omission leaves the existing workdir fragment
unchanged. Selector strings use JSON escaping, including brace escapes, so
authored glob syntax remains literal prompt content.

For harness-routed `!`/`!!` work, the extension echoes the private command route
and immutable request fields through transient
`shell.command_progress_reported` / `shell.command_finished_reported` events.
It never authors the canonical event names. The harness commits and validates
these reports before mapping the private route back to the UI lifecycle id and
publishing canonical progress/completion. See
[SPEC-shell-command-reports-and-canonical-facts](../../../specs/SPEC-shell-command-reports-and-canonical-facts.md).

## Scheduler and shutdown ordering

Tool invocations run through a fixed native-thread `WorkScheduler` with bounded
priority queues. The scheduler owns worker threads and sender clones used to
publish protocol messages. Dropping the scheduler is therefore a deterministic
shutdown boundary: queued work is discarded, workers are woken, and already
running jobs are joined before drop returns.

Every admitted model tool call except the dedicated workdir-setter transaction
has one bounded cancellation lifecycle from scheduler enqueue through terminal
reporting. Scheduler dequeue, directory-lock waiting and acquisition, and
dispatch transfer the same lifecycle authority. Cancellation and effect start
race through one atomic transition: cancellation first emits one cancelled
terminal and prevents process spawn or filesystem mutation; effect start first
retains active shell/search cancellation without promising rollback. Terminal
reporting removes the live registry entry rather than retaining call-id
tombstones. UI `!` / `!!` commands keep their separate cancellation path.

Long-running read-only search tools that run after dequeuing (`grep` / `find`)
also register cancellation handles while active. Tool cancellation and runtime
shutdown signal those handles so a running ripgrep child or filesystem traversal
can stop before scheduler drop waits for worker threads to exit.

At every session or process shutdown path, including explicit
`session_shutdown`, `disconnect`, EOF, reader decode errors, and output
shutdown errors, ext-shell must shut down `DirLockManager` and cancel queued or
running work. This releases manual locks and cancels queued lock waiters so
worker jobs blocked in lock acquisition can exit. On process termination paths,
only after this cleanup should `WorkScheduler` be dropped and the tau-client
manual runtime be finished so the protocol writer can flush and join.

## Tool tags

`tau-ext-shell` tags tools with neutral capability metadata such as
`shell:edit:line`, `shell:edit:replace`, `shell:edit:apply_patch`, `shell:exec:generic`,
`shell:exec:shell_command`, and `shell:workdir`. The extension must not decide which
model gets which
surface; `workdir` carries `shell:workdir`. The harness interprets these tags together with provider-published
model tags and role configuration. The exact-text implementation retains its
internal `replace` identity for configuration and extension lifecycle events,
but advertises the provider-visible `edit` alias. Its routed `tool.started`
and terminal reports therefore use `replace`, while its provider definition,
model call, and canonical terminal use `edit`. The line-coordinate
implementation also remains internally `edit`, so prompt construction rejects
a policy that enables both implementations together.

## Skill and instruction discovery

`tau-ext-shell` discovers local AGENTS.md files and Markdown skills from the
working-directory and user roots, parses skill frontmatter through `tau-skills`,
canonicalizes file paths, and publishes complete atomic source snapshots. User
AGENTS.md roots are scanned before project roots in this order:
`$HOME/.config/agents`, `$HOME/.config/agents.local`, legacy `$HOME/.agents`,
and legacy `$HOME/.agents.local`. All readable, non-empty files from those roots
are stacked; the XDG roots do not suppress legacy roots. AGENTS.md roots and
candidates are trusted prompt input, and discovery follows symlinks in both user
and project roots.

Project skill roots stay first and are discovered from ancestor
`.agents/skills` and `.agents.local/skills` directories. User skill roots follow
as `$HOME/.config/agents/skills`, `$HOME/.config/agents.local/skills`, legacy
`$HOME/.agents/skills`, and legacy `$HOME/.agents.local/skills`. The shell
extension marks XDG user skill roots with higher source precedence than legacy
user skill roots so duplicate user skill names prefer XDG before modified time.
Skill roots, nested skill directories, root-level Markdown skill files, and
directory-level `SKILL.md` files are followed through symlinks; `tau-skills`
tracks canonical directories during traversal so symlink cycles stop at the first
already-seen directory.
Because the shell extension registers as a session context provider, it publishes
one complete session snapshot followed by `extension.session_context_ready` for
each `session.started`. For every `session.agent_loaded`, it publishes one
complete snapshot correlated to that load's `agent_initialization_id`, then
correlated workdir context and readiness. Replay defers this sequence until the
per-agent replay boundary so restored metadata wins over process defaults.
Skill collision diagnostics belong to session discovery and are not repeated for
per-agent snapshots.
All discovery publications use `persist=false` metadata and the ordering required by
[SPEC-session-discovery-declarations-and-readiness](../../../specs/SPEC-session-discovery-declarations-and-readiness.md).
Its startup `shell.workdir` prompt-fragment event is a transient declaration that
the harness commits before activating prompt assembly state, as specified by
[SPEC-prompt-fragment-declarations-and-projection](../../../specs/SPEC-prompt-fragment-declarations-and-projection.md).
`tau-skills` deduplicates files before snapshot publication; the
harness still owns skill-name validation at the protocol boundary, canonical
winner selection among announced candidates from all sources, model/user
invocation filtering, and `:skill` prompt expansion policy.
