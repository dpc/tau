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

## Tool tags

`tau-ext-shell` tags tools with neutral capability metadata such as
`shell:edit:line`, `shell:edit:apply_patch`, `shell:exec:generic`,
`shell:exec:shell_command`, and `shell:cd`. The extension must not decide which
model gets which
surface; the harness interprets these tags together with provider-published
model tags and role configuration.

## Skill and instruction discovery

`tau-ext-shell` discovers local AGENTS.md files and Markdown skills from the working-directory and user skill roots, parses skill frontmatter through `tau-skills`, canonicalizes file paths, and announces candidates to the harness. The extension is only the filesystem discoverer: the harness owns skill-name validation at the protocol boundary, collision winner selection, model/user invocation filtering, and `/skill` prompt expansion policy.
