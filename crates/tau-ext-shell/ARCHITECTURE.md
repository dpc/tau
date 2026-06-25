# tau-ext-shell architecture

`tau-ext-shell` owns Tau's local filesystem and subprocess tools. It must avoid
process-global cwd changes after startup: concurrent tool workers resolve paths
against per-agent state instead.

## Per-agent cwd metadata

The extension instance name from `configure.instance_name` defines the cwd
metadata key: `ext_<instance>_cwd`. The built-in core shell instance therefore
uses `ext_core-shell_cwd`. If multiple shell instances are configured, each uses
its own instance-derived key and keeps an independent cwd map.

Committed `agent.metadata_set` / `agent.metadata_unset` events are the source of
truth. The extension updates its in-memory `CwdState` only after seeing those
events, publishes fresh `agent_context.cwd` after each committed change, and
emits `extension.context_ready` only after publishing the initial cwd context for
a loaded agent. Metadata values are inheritable so child agents start in the
parent's remembered cwd.

## Cwd-aware tools and locks

The `cd` tool changes the remembered cwd by emitting `agent.metadata_set` and a
model-visible `agent.user_message_injected` notice. Explicit `cwd` arguments on
shell tools also emit metadata and update remembered cwd. Relative paths for
filesystem tools (`read`, `edit`, `find`, `grep`, `ls`, `apply_patch`, and
`dir_lock`) are resolved against the remembered cwd before execution or automatic
lock selection. Once automatic lock selection begins, the invocation carries the
same cwd snapshot through lock waiting and execution, even if committed cwd
metadata changes before the lock is granted. This keeps locks, shell execution,
and patch paths aligned without calling `chdir(2)` in the extension process.

Directory locking is opt-in. When disabled, `shell` / `gpt_shell` calls are
ordinary read-write commands and no access-mode chip is published for UI display.
When enabled, shell access mode is inferred from manual lock ownership: a command
whose cwd is covered by the caller's manual lock is read-write and takes an
automatic lock; otherwise it is read-only.

The default directory-lock backend is `memory`, which coordinates only workers
inside one ext-shell process. The optional `filesystem` backend stores the same
manual locks, automatic locks, and FIFO waiters in a JSON registry protected by
`fs2` file locks. Each ext-shell instance holds an exclusive lease lock under the
registry's `instances/` directory; other instances reap registry records whose
lease locks are no longer held. The filesystem backend therefore coordinates
Tau/ext-shell processes on the same host and user account without treating
timestamps as liveness proof. Filesystem `instance_id`s are internal lease
identifiers; model/user-visible diagnostics and `owner_agent_id` recovery use
only `AgentId`. The backend is initialized only when directory locking is
enabled. Backend reconfiguration initializes the requested backend before
swapping it in, so initialization failure is reported as `ConfigError` while the
previous backend and its lock state remain active. Backend swaps are also
rejected while automatic locks are active, because those guards release through
the backend that granted them and must remain visible to later acquisitions.

The read-write inference and automatic lock acquisition happen under the
`DirLockManager` state lock. A shell call queued as read-write must still have
covering manual-lock ownership at the moment the automatic lock is granted;
otherwise it falls back to read-only execution instead of running under stale
coverage.

## Scheduler and shutdown ordering

Tool invocations run through a fixed native-thread `WorkScheduler` with bounded
priority queues. The scheduler owns worker threads and sender clones used to
publish protocol messages. Dropping the scheduler is therefore a deterministic
shutdown boundary: queued work is discarded, workers are woken, and already
running jobs are joined before drop returns.

At every post-scheduler termination path, including explicit
`session_shutdown`, `disconnect`, EOF, reader decode errors, and response-send
errors, ext-shell must shut down `DirLockManager` before dropping
`WorkScheduler`. This releases manual locks and cancels queued lock waiters so
worker jobs blocked in lock acquisition can exit. Only after scheduler drop
should the main response sender be dropped and the protocol writer thread
joined.

## Tool tags

`tau-ext-shell` tags tools with neutral capability metadata such as
`shell:edit:line`, `shell:edit:apply_patch`, `shell:exec:generic`,
`shell:exec:shell_command`, and `shell:cd`. The extension must not decide which
model gets which
surface; the harness interprets these tags together with provider-published
model tags and role configuration.

## Skill and instruction discovery

`tau-ext-shell` discovers local AGENTS.md files and Markdown skills from the
working-directory and user roots, parses skill frontmatter through `tau-skills`,
canonicalizes file paths, and announces candidates to the harness. User
AGENTS.md roots are scanned before project roots in this order:
`$HOME/.config/agents`, `$HOME/.config/agents.local`, legacy `$HOME/.agents`,
and legacy `$HOME/.agents.local`. All readable, non-empty files from those roots
are stacked; the XDG roots do not suppress legacy roots.

Project skill roots stay first and are discovered from ancestor
`.agents/skills` and `.agents.local/skills` directories. User skill roots follow
as `$HOME/.config/agents/skills`, `$HOME/.config/agents.local/skills`, legacy
`$HOME/.agents/skills`, and legacy `$HOME/.agents.local/skills`. The shell
extension marks XDG user skill roots with higher source precedence than legacy
user skill roots so duplicate user skill names prefer XDG before modified time.
`tau-skills` deduplicates files found by this extension before announcement; the
harness still owns skill-name validation at the protocol boundary, canonical
winner selection among announced candidates from all sources, model/user
invocation filtering, and `/skill` prompt expansion policy.
