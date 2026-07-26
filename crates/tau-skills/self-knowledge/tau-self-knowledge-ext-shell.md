---
name: tau-self-knowledge-ext-shell
description: Use this extension skill when the user asks about Tau's core-shell extension, filesystem tools, shell command execution, file editing, directory locks, AGENTS.md discovery, shell configuration, or read-only tool isolation.
advertise: false
---

# Tau core-shell extension self-knowledge

`core-shell` is Tau's built-in shell and filesystem extension. It runs `tau-ext-shell`, is enabled by default, and registers the everyday project-inspection and mutation tools used by agents.


## Tools and behavior

Model-visible tools:

- `read` — reads UTF-8 and non-UTF-8 files with line numbers, line-ending markers, Unicode replacement for invalid bytes plus `invalid-utf8` flags, range/ranges support, line/byte truncation metadata, a 10 MiB input safety cap, a rendered-range expansion cap that can reject large overlapping multi-range requests before rendering, and a bounded nearby-sibling suggestion for simple missing-path typos.
- `read_image` — reads one local PNG, JPEG, or WebP under the same filesystem authority as `read`, validates and re-encodes it under strict byte/dimension/pixel/decoded-memory limits, strips source metadata, and returns bounded high-detail typed image content. Bare calls keep the 2048-side/2,500-patch high profile. Explicit experimental `mode: "overview"` uses 1024-side/600-patch local preparation for coarse inspection only. An optional half-open `region` uses EXIF-oriented source pixels and crops before profile resizing. It is visible only when the exact provider route publishes native image tool-result support (initially GPT-5.6 ChatGPT Responses). Generic UI/debug output shows source/oriented/region/output geometry, profile, patches, format, and byte count, never pixels or base64.
- `edit` — applies context-checked line-oriented replacements. `newText` fully replaces the 1-based half-open `start_line`..`end_line_exclusive` range; `start_line` is included and `end_line_exclusive` is excluded. Empty insertion ranges use `start_line == end_line_exclusive`, such as `1..<1` for top-of-file insertion or `total_lines + 1 ..< total_lines + 1` for EOF append. Each edit has a `context_line` that matches the original content of `start_line`; use an empty `context_line` when `start_line` is the append slot past the end of the file. Non-empty `newText` with no trailing line ending is normalized into a full line; explicit line endings are preserved, so callers can create mixed endings. Context mismatches report `context_line_number` as the requested `start_line`. The agent-visible result is minimal status only; the UI receives a separate structured diff payload for changed UTF-8 files, including inline changed-token segments.
- `apply_patch` — applies patch-style file edits and also sends structured UI-only diffs for changed UTF-8 files. It carries the neutral `shell:edit:apply_patch` tag; the harness built-in ChatGPT policy re-enables it after disabling the broader `shell:*` family.
- `shell` — runs `sh -c`-style commands with optional call-local `cwd`, timeout, stdout/stderr capture, Unicode replacement for invalid output bytes plus `invalid-utf8` flags, truncation, and tool cancellation support. Its `cwd` never changes remembered state. It is the generic shell execution alternative.
- `workdir` — with no path, reads this shell instance's current per-agent path/status; with a path, validates, canonicalizes, commits, and persistently changes it. State uses inheritable instance-scoped metadata (for the built-in shell, `ext_core-shell_cwd`). Dependent shell/filesystem calls belong in a later turn after a setter succeeds. It carries `shell:workdir`.
- `gpt_shell` — shell-like execution surface advertised as model-visible `shell_command` for GPT-style tool compatibility. Its optional call-local `workdir` resolves from the remembered persistent workdir and never changes later calls; the separate top-level `workdir(path)` tool reads or changes persistent state, and dependent calls must occur after a successful setter in a later turn. It does not accept the removed GPT `cwd` spelling. It carries the neutral `shell:exec:shell_command` tag; the harness built-in ChatGPT policy re-enables it after disabling the broader `shell:*` family.
- `grep` — ripgrep-backed literal or regex search with context, glob filtering, truncation, escaped control characters in paths, invalid-UTF-8 path markers for byte paths, `limit` capped at 2000 matches, and `context` capped at 20 lines.
- `find` — ignore-aware glob file search with escaped control characters in paths, invalid-UTF-8 path markers, and `limit` capped at 2000 results.
- `ls` — sorted directory listing with 1-based entry prefixes, escaped control characters/backslashes, Unicode replacement for invalid filename bytes plus `invalid-utf8` flags, `limit` capped at 2001 entries, and standard truncation metadata. When `limit_reached` is true, entries are a bounded filesystem-order sample sorted for display rather than a complete alphabetic prefix.
- `dir_lock` — manual directory update lock/unlock for coordinating mutating agents.

Test builds or the `echo-agent` cargo feature also register `echo` for harness tests.

Every filesystem, shell, lock, and user `!`/`!!` invocation snapshots its
instance workdir at admission. Queued or lock-waiting work does not drift after
a later workdir commit. Stale remembered paths are retained and fail closed;
use an absolute `workdir` setter to repair them. Each configured ext-shell
instance initializes only a missing metadata key from its frozen actual process
startup cwd and remains independent of other instances.

When `workdir` is visible, Tau's dynamic shell guidance normally directs the
agent to set the matching instance's workdir to the project root before project
work. That path becomes the cwd/base for later shell and filesystem calls from
that instance only. It can select configured directory-scoped wrappers such as
`direnv exec .` and affects other cwd-sensitive wrappers/tools; Tau does not
assume such a wrapper is enabled. A dependent call must follow a successful
setter in a later tool turn because sibling calls have no workdir-first
ordering. Prefixed shell instances show their matching prefix and independent
path/status, and hidden workdir capabilities do not emit this guidance.

User `!`/`!!` commands are routed to exactly one generic shell instance. They
fail without execution when none is available, when several are ambiguous, or
when the target session/agent workdir is not ready.

On Linux and Android, the model-visible `shell` / `shell_command` surfaces and user
`!`/`!!` commands use independent stdout/stderr PTYs. Commands detect TTY output
descriptors while stream identity remains intact. Stdin stays closed because the
surfaces do not accept interactive input. Other platforms retain closed stdin
and output pipes. Both model and user shell children normally override `PAGER`,
`GIT_PAGER`, `GH_PAGER`, `JJ_PAGER`, and `SYSTEMD_PAGER` with `cat` after
ordinary `shell.extra_env`, while preserving `TERM`. Setting
`shell.non_interactive_pager: false` is the explicit opt-out from this
protection. `MANPAGER`, `BAT_PAGER`, and arbitrary application-specific pager
variables remain ordinary. The protected `cat` must resolve through the child's
effective `PATH`.

`tau-ext-shell` runs tool work through a bounded priority scheduler. Short bursts can queue instead of failing immediately when workers are busy; queued model tool calls can be canceled before they start; user `!` shell work and control-sensitive `dir_lock` calls have higher-priority lanes than bulk model work. If bounded queue or queued-argument budgets are exhausted, the tool reports a clear backpressure error instead of spawning unbounded threads.

Selected shell tools such as `read` and `edit` attach compact repair examples to
their tool metadata. These examples are not included in normal model tool
definitions; the harness may show one bounded example only after a failed call.

For Tau VCR runs, `ls`, `read`, `edit`, `apply_patch`, `shell`, and `gpt_shell` use the shared ext-shell world-operation recorder. Replay substitutes recorded filesystem effects such as directory listing, file reads, parent-path checks, directory creation, and asserted writes/removes while still running normal tool argument handling, context-line validation, patch application, diff generation, escaping, invalid-UTF-8 handling, and truncation logic. Shell terminal outcomes are recorded as world operations: finished results replay at 100x recorded speed, while recorded cancellations require a matching replay cancellation request.


## Directory locks and mutation safety

`config.dir_lock.enable` defaults false. When it is true, `dir_lock` is available and mutating `edit` / `apply_patch` calls automatically acquire matching directory locks. `shell` / `gpt_shell` calls are inferred read-write only while the agent holds a manual lock covering the command's call-local `cwd` / `workdir`; otherwise they are inferred read-only and do not wait on update locks. When directory locking is disabled, all shell calls run read-write and the UI does not show an access-mode chip. The extension publishes a `:shell-dir-force-unlock DIRECTORY` user action when a manual lock blocks work long enough to matter. `config.dir_lock.backend` defaults to `"memory"` for process-local coordination; set it to `"filesystem"` with an optional private `state_dir` to coordinate locks across Tau/ext-shell processes on the same host and user account.

Inferred read-only shell mode is advisory unless `config.dir_lock.enforce_ro_bind: true` is set while directory locking is enabled. The read-only bind defaults true under `dir_lock`; when enabled, Tau requires a read-only bind mount of the tool cwd and fails the shell call if native isolation is unsupported or cannot be installed.


## Agent context discovery

`core-shell` discovers and publishes project/user instructions and skills:

- `AGENTS.md` and `AGENTS.*.md` from `$HOME/.config/agents/`,
  `$HOME/.config/agents.local/`, legacy `$HOME/.agents/`, then legacy
  `$HOME/.agents.local/`; both XDG and legacy user files are stacked when present
- `AGENTS.md` and `AGENTS.*.md` in current-working-directory ancestors, plus each
  ancestor's matching `.agents.local/AGENTS.md` and `.agents.local/AGENTS.*.md`
- skills under project `.agents/skills` and `.agents.local/skills`, followed by
  `$HOME/.config/agents/skills`, `$HOME/.config/agents.local/skills`, legacy
  `$HOME/.agents/skills`, and legacy `$HOME/.agents.local/skills`
- duplicate user-skill names from XDG skill roots beat legacy user roots before
  modified-time collision resolution

`tau-ext-shell` parses skill `user-invocable`, `disable-model-invocation`, and
`argument-hint` metadata and publishes one complete source snapshot at session
discovery and for each correlated agent initialization. The harness atomically
selects collision winners and freezes each initialized agent's view.
`disable-model-invocation` hides a skill from that agent's
`<available_skills>` and model `skill` tool and implies user invocation, while
`:skill <name> [args]` (or `:skill:<name> [args]`) expands against the selected
agent's frozen snapshot.

`.local` locations are intended for machine- or user-specific instructions and are usually gitignored.

AGENTS.md files and skills are trusted prompt input. Tau follows symlinks during
discovery; do not run Tau in projects whose instruction files or skill files you
do not trust.


## Configuration

Configured under `extensions.core-shell.config`:

```json5
extensions: {
  "core-shell": {
    config: {
      working_directory: "/srv/project",
      shell: {
        command: "bash",
        prefix: ["nix", "develop", "-c"],
        user_command_timeout_secs: 3600,
        non_interactive_pager: true,
        extra_env: { PATH: "/custom/bin:/usr/bin" },
      },
      dir_lock: { enable: false, backend: "memory", enforce_ro_bind: true },
    },
  },
}
```

`working_directory` changes the extension process cwd only during startup config and therefore its missing-key fallback; it never overrides restored per-agent state. Late changes after runtime events are rejected. `shell.command` is invoked as `<command> -c <user command>` after `shell.prefix`. `shell.extra_env` is applied to shell-tool and user `!`/`!!` child processes after the inherited environment; empty values remove variables from the child environment. The protected pager overlay follows `extra_env` unless `shell.non_interactive_pager` is explicitly false. `user_command_timeout_secs` affects UI-initiated shell commands; agent tool calls use their own `timeout` argument.
