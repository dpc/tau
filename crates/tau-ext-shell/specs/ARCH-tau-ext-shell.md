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
[DECISION-per-agent-extension-workdirs](../../../specs/DECISION-per-agent-extension-workdirs.md).

## tau-client runtime boundary

The shell extension uses `tau-client` for protocol startup, exact
subscriptions, configuration error reporting, replay/live dispatch filtering,
and serialized output. Shell-specific policy remains local: cwd folding,
session/agent lifecycle cleanup, directory-lock scheduling, tool cancellation,
and `StartAgentResult` correlation are owned by `ShellRuntime`.

Worker and scheduler output goes through a shell-local `Output` adapter backed
by `ClientHandle::send_detached` in production. This preserves the historical
enqueue-to-writer behavior: worker threads do not block on protocol flush, while
the tau-client writer still reports encode/flush failures during graceful
shutdown. Configure-time tool re-registration deliberately uses synchronous
`register_local_tool`: tau-client buffers that declaration behind static startup
defaults and flushes the configured override before `Ready`. Tests can use an
mpsc-backed adapter for direct state-machine coverage.

## User-shell reports

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
`shell:edit:line`, `shell:edit:apply_patch`, `shell:exec:generic`,
`shell:exec:shell_command`, and `shell:workdir`. The extension must not decide which
model gets which
surface; `workdir` carries `shell:workdir`. The harness interprets these tags together with provider-published
model tags and role configuration.

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
All discovery publications use `persist=false` metadata and the ordering required by
[SPEC-session-discovery-declarations-and-readiness](../../../specs/SPEC-session-discovery-declarations-and-readiness.md).
Its startup `shell.workdir` prompt-fragment event is a transient declaration that
the harness commits before activating prompt assembly state, as specified by
[SPEC-prompt-fragment-declarations-and-projection](../../../specs/SPEC-prompt-fragment-declarations-and-projection.md).
`tau-skills` deduplicates files before snapshot publication; the
harness still owns skill-name validation at the protocol boundary, canonical
winner selection among announced candidates from all sources, model/user
invocation filtering, and `:skill` prompt expansion policy.
