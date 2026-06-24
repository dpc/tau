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
- `edit` — applies context-checked line-oriented replacements. `newText` fully replaces the 1-based half-open `start_line`..`end_line_exclusive` range; `start_line` is included and `end_line_exclusive` is excluded. Empty insertion ranges use `start_line == end_line_exclusive`, such as `1..<1` for top-of-file insertion or `total_lines + 1 ..< total_lines + 1` for EOF append. Each edit has a `context_line` that matches the original line immediately before `start_line`; use an empty `context_line` when `start_line` is 1, and use the original last line for EOF appends to non-empty files. Non-empty `newText` with no trailing line ending is normalized into a full line; explicit line endings are preserved, so callers can create mixed endings. BOF context mismatches report `context_line_number: 0`. The agent-visible result is minimal status only; the UI receives a separate structured diff payload for changed UTF-8 files, including inline changed-token segments.
- `apply_patch` — applies patch-style file edits and also sends structured UI-only diffs for changed UTF-8 files. It carries the neutral `shell:edit:apply_patch` tag; the harness built-in ChatGPT policy re-enables it after disabling the broader `shell:*` family.
- `shell` — runs `sh -c`-style commands with optional `cwd`, timeout, stdout/stderr capture, Unicode replacement for invalid output bytes plus `invalid-utf8` flags, truncation, and tool cancellation support. It is the generic shell execution alternative. It no longer accepts an explicit `ro` / `rw` argument.
- `cd` — changes the shell extension's remembered working directory for the current agent. The cwd is also stored as inheritable agent metadata using the extension instance key (for the built-in shell, `ext_core-shell_cwd`). It carries `shell:cd` and remains available under the built-in ChatGPT shell policy.
- `gpt_shell` — shell-like execution surface advertised as model-visible `shell_command` for GPT-style tool compatibility. It carries the neutral `shell:exec:shell_command` tag; the harness built-in ChatGPT policy re-enables it after disabling the broader `shell:*` family.
- `grep` — ripgrep-backed literal or regex search with context, glob filtering, truncation, escaped control characters in paths, invalid-UTF-8 path markers for byte paths, `limit` capped at 2000 matches, and `context` capped at 20 lines.
- `find` — ignore-aware glob file search with escaped control characters in paths, invalid-UTF-8 path markers, and `limit` capped at 2000 results.
- `ls` — sorted directory listing with 1-based entry prefixes, escaped control characters/backslashes, Unicode replacement for invalid filename bytes plus `invalid-utf8` flags, `limit` capped at 2001 entries, and standard truncation metadata. When `limit_reached` is true, entries are a bounded filesystem-order sample sorted for display rather than a complete alphabetic prefix.
- `dir_lock` — manual directory update lock/unlock for coordinating mutating agents.

Test builds or the `echo-agent` cargo feature also register `echo` for harness tests.

`tau-ext-shell` runs tool work through a bounded priority scheduler. Short bursts can queue instead of failing immediately when workers are busy; queued model tool calls can be canceled before they start; user `!` shell work and control-sensitive `dir_lock` calls have higher-priority lanes than bulk model work. If bounded queue or queued-argument budgets are exhausted, the tool reports a clear backpressure error instead of spawning unbounded threads.

Selected shell tools such as `read` and `edit` attach compact repair examples to
their tool metadata. These examples are not included in normal model tool
definitions; the harness may show one bounded example only after a failed call.

For Tau VCR runs, `ls`, `read`, `edit`, `apply_patch`, `shell`, and `gpt_shell` use the shared ext-shell world-operation recorder. Replay substitutes recorded filesystem effects such as directory listing, file reads, parent-path checks, directory creation, and asserted writes/removes while still running normal tool argument handling, context-line validation, patch application, diff generation, escaping, invalid-UTF-8 handling, and truncation logic. Shell terminal outcomes are recorded as world operations: finished results replay at 100x recorded speed, while recorded cancellations require a matching replay cancellation request.


## Directory locks and mutation safety

`config.dir_lock.enable` defaults false. When it is true, `dir_lock` is available and mutating `edit` / `apply_patch` calls automatically acquire matching directory locks. `shell` / `gpt_shell` calls are inferred read-write only while the agent holds a manual lock covering the command cwd; otherwise they are inferred read-only and do not wait on update locks. When directory locking is disabled, all shell calls run read-write and the UI does not show an access-mode chip. The extension publishes a `/shell-dir-force-unlock DIRECTORY` user action when a manual lock blocks work long enough to matter. `config.dir_lock.backend` defaults to `"memory"` for process-local coordination; set it to `"filesystem"` with an optional private `state_dir` to coordinate locks across Tau/ext-shell processes on the same host and user account.

Inferred read-only shell mode is advisory unless `config.dir_lock.enforce_ro_bind: true` is set while directory locking is enabled. The read-only bind defaults true under `dir_lock` and uses a read-only bind mount of the tool cwd when supported.


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

`tau-ext-shell` parses skill `user-invocable`, `disable-model-invocation`, and `argument-hint` metadata and forwards it to the harness. The harness owns collision winner selection and policy: `disable-model-invocation` hides a skill from `<available_skills>` and the model `skill` tool and implies user invocation, while `/skill <name> [args]` (or `/skill:<name> [args]`) explicitly injects user-invocable skill content into the next prompt with arguments appended.

`.local` locations are intended for machine- or user-specific instructions and are usually gitignored.


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
        extra_env: { PAGER: "cat" },
      },
      dir_lock: { enable: false, backend: "memory", enforce_ro_bind: true },
    },
  },
}
```

`working_directory` changes the extension process cwd only during startup config; late changes after runtime events are rejected. `shell.command` is invoked as `<command> -c <user command>` after `shell.prefix`. `shell.extra_env` is applied to shell-tool and user `!`/`!!` child processes after the inherited environment; empty values remove variables from the child environment. `user_command_timeout_secs` affects UI-initiated shell commands; agent tool calls use their own `timeout` argument.
