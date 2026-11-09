---
name: tau-self-knowledge-ext-shell
description: Use when using or configuring Tau core-shell filesystem and shell tools, including command execution, file editing, directory locks, AGENTS.md discovery, shell configuration, or read-only tool isolation.
advertise: false
---

# Tau core-shell extension self-knowledge

`core-shell` runs `tau-ext-shell` and is enabled by default.

## Tools

- `read`: numbered file reads, including invalid UTF-8 markers, ranges, truncation
  metadata, and missing-path suggestions. Input cap: 10 MiB; overlapping ranges
  can exceed the rendered-expansion cap.
- `export`: uploads one regular file or in-process tar+zstd directory bundle and
  returns only `kind` and `<tau-artifact:FULL_KEY>` in the `artifact` header.
  Optional `mime_type` overrides file extension inference. The bounded filename
  and effective MIME persist immutably and participate in the artifact identity.
  `FULL_KEY` has exactly 24 lowercase hex digits (96-bit BLAKE3), without an
  algorithm label. Old full-digest reference syntax is rejected.
  MIME is declared or inferred, not verified. Export renews retention even for
  duplicate bytes and metadata; originals outlive ephemeral transcripts. Stored and expanded
  bytes each default to 32 MiB under `artifact_limits` in `harness.yaml`, with
  a 512 MiB configuration ceiling. Directory MIME is fixed; directory roots and
  members cannot be symlinks or special files. Use a quiescent source tree.
- `import`: verifies bytes and metadata, returns `kind`, `path`, `size` and recorded
  MIME when present. A safe filename is restored inside a fresh mode-0700 private
  parent with a mode-0600 child; legacy or unusable names get a generic private file.
  Pass the bare 24-digit lowercase hex key; plain `tau-artifact:KEY` URIs and
  Markdown `<tau-artifact:KEY>` references also work. No other arguments are needed.
  Import recognizes versioned directory bundles by their bytes and extracts them
  into a private mode-0700 directory.
  It extracts only regular files and directories, without executable permissions.
  Results include `kind: file|directory`. Import does not renew retention or
  execute content. Pass image references directly to `read_image` for inspection.
- `edit`: normally the exact-text `replace` implementation. All `oldText` targets
  must match once in the original UTF-8 snapshot and cannot overlap. Matching
  normalizes CRLF/CR and ignores an initial BOM; writes preserve untouched bytes,
  BOM, and local newline style. Results contain `edits`, `changed`, `total_bytes`.
- Legacy `edit`, selected with `shell:tool-style:edit`: replaces the 1-based
  half-open `start_line`..`end_line_exclusive` range with `newText`. Equal endpoints
  insert; EOF append uses `total_lines + 1` at both endpoints. `context_line`
  must match the original preceding line, or be empty at file start. Nonempty
  replacement text without a final line ending becomes a full line.
- `apply_patch`: patch-style edits. Editing tools send separate UI-only diffs;
  oversized diffs may be omitted without changing the edit outcome.
- `shell`: command execution with call-local `cwd`; timeout defaults to 300 seconds.
- `gpt_shell`, visible as `shell_command`: equivalent GPT-style surface with
  call-local `workdir`, not `cwd`.
- `workdir`: reads the instance-local persistent cwd, or validates, canonicalizes,
  and commits a new one. A getter returns `path` without a routine availability
  header; it still reports degraded availability. This is separate from either
  command's call-local override.
- `grep`: in-process ripgrep search; no external `rg` or `RIPGREP_CONFIG_PATH`.
  `limit` caps at 2000 matches; `context` at 20 lines. It searches regular files
  and directories, not special files.
- `find`: ignore-aware glob search, at most 2000 results.
- `ls`: sorted listing, at most 2001 entries. With `limit_reached`, entries are a
  sorted filesystem-order sample, not a complete alphabetical prefix.
- `dir_lock`: manual directory update reservation.

The neutral `shell:exec:shell_command` and `shell:edit:apply_patch` tags let the
built-in ChatGPT policy re-enable those tools after disabling `shell:*`.

## Workdirs and execution

Set the matching instance's `workdir` to the project root before project work.
Run dependent calls in a later tool turn after success: sibling calls have no
workdir-first ordering. Each instance has independent, inheritable/restored
per-agent state (`ext_core-shell_cwd` for the built-in instance).

Every call snapshots its cwd at admission, including queued/lock-waiting calls.
Call-local overrides never change persistent state. Stale paths fail closed;
repair with an absolute setter. Cwd can select directory-scoped wrappers such as
`direnv exec .`; wrappers do not virtualize file discovery or completion.

User commands require `!instance command` or `!!instance command`; there is no
default instance. `!` appends attributed output without activating inference;
`!!` does not append. Headless `tau send` supports only `!!`. An acceptance timeout
leaves the outcome unconfirmed: do not blindly retry. Running user commands keep
idle sessions alive after UI detach, but not through explicit shutdown/crash.

On Linux, Android, and macOS, shell stdout/stderr use independent PTYs. Stdin
stays closed; commands cannot accept interactive input. Tau sets `PAGER`,
`GIT_PAGER`, `GH_PAGER`, `JJ_PAGER`, and `SYSTEMD_PAGER` to `cat` unless
`shell.non_interactive_pager: false`. Other pager variables remain unchanged.
Commands may queue; exhausted queue budgets return backpressure errors.

## Output limits

Model shell output is an ordered source stream, separate from completion
receipts. Pending output flushes after 15 seconds of inactivity or 30 seconds
from its first pending bytes, whichever comes first; silence sends no update and size
limits never cause an early flush. Actual completion flushes the final tail.
Background placeholders expose `status: running-in-background` and the canonical
`call_id` so you can wait for or cancel that call before source arrives. A
successful cancellation request returns empty text; completion remains separate.
An ordinary process exit reports `termination_reason: exit` and `exit_code`,
not `status`.
Streamed `out`/`err` source-line segments reuse the ordinary `crlf`, `cr`, and
`no_nl` markers. `no_nl` marks the absence of a source newline at the segment boundary, not
process EOF. Later output from the same pipe can continue the segment across
updates or intervening output from the other pipe. A CR held across a batch
boundary may complete a previous segment with an empty `crlf` or `cr` segment.
The agent can react while the process is alive, but output never interrupts
active inference. `wait(complete_only: true)` masks nonterminal tool wakeups for
that agent while preserving capture and unrelated activating input.

Accepted updates are limited to 8 KiB each and 64 KiB/64 updates per call, with a
separate bounded final diagnostic capture. Tau drains excess output, retains
recent text, and reports omissions; overflow does not cancel the command.
`wait` returns a successful receipt with `cause` (the source call ID) and
`outcome` (`completed`, `error`, or `canceled`), without a copy of the output.
The `tool_output` tool can page retained accepted text, but cannot recover
omitted raw output.
Inspect oversized shell output using
ordinary shell commands and the terminal's temporary-file path header. Existing
best-effort compaction transcript archives support historical inspection of
accepted text; neither route recovers omitted raw bytes or expired files.

Inspection/edit-recovery/user-command surfaces cap visible output at 10 KiB;
model `shell`/`shell_command` additionally cap their producer-side aggregate at
15 KiB and, where applicable, 2,000 lines. The harness's bounded final source
capture is separate from that aggregate.

Native truncation includes totals. Completed commands also expose saved output
when the smaller final harness capture omits bytes or earlier live batches
leave only a terminal tail. Saved and delivered records preserve the observed
pipe-segment order, which does not establish cross-pipe write chronology.
The saved rendering is capped at 16 MiB:
`full_output_path` means complete; `saved_output_path` with
`saved_output_truncated: true` and `saved_output_bytes` means incomplete.
`saved_output_unavailable: true` means no private saved file could be created.
Files expire after both 32 later relevant calls and about 15 minutes, with cleanup
on the next relevant call; graceful shutdown removes them.
Compact tool rows mark truncation or limits. Full tool displays add selected
recovery paths and completeness facts without repeating captured output.

## Directory locks

`dir_lock.enable` defaults false. When enabled, file edits acquire automatic
locks. Shell calls become read-write only while the agent holds a manual lock
covering the effective cwd; otherwise they are read-only and do not wait on update
locks. With locking disabled, shell calls run read-write.

`dir_lock.enforce_ro_bind` defaults true: while locking is enabled, read-only
shell calls require a read-only bind mount of cwd and fail if isolation cannot
be installed. Disabling it makes read-only mode advisory. Backend `"memory"`
coordinates one process; `"filesystem"` with optional private `state_dir`
coordinates same-user processes on one host.

Manual reservations and running tools' automatic guards have separate lifetimes.
`dir_lock(unlock)`, owner unlock, and `:shell-dir-force-unlock DIRECTORY` release
only manual reservations. They neither cancel tools nor release running guards;
backgrounding or changing cwd does not move those guards.

Tau releases a delegate/side agent's manual reservation when its tracked start
operation finishes, even if the agent remains loaded. Reacquire before resumed
mutations; a recovery summary does not prove ownership.

For immutable long-running work that needs no checkout, launch the built
executable from a dedicated task directory outside the source tree, with outputs
and any required manual write lock there. Do not use another cwd to hide checkout
writes. Ancestor/descendant paths overlap: editing directly under `/tmp/public`
can conflict with a writer in a child directory.

`apply_patch` locks source and destination paths, including absolute targets.
Create dedicated scratch parents first; a missing parent may cause a lock on its
deepest existing ancestor. Split unrelated source/scratch edits.

Lock-wait progress names requested directories and, when known, the blocking
owner/directory/kind. Contact that owner and check running work before unlocking.
Blocked explicit `dir_lock(update)` also emits one nonterminal header-only source
with `status: waiting` and a bounded admission snapshot: `blocker_agent`,
`blocker_directory`, and `blocker_kind` (`manual_lock`, `automatic_lock`, or
`earlier_queued_request`). It names one actual overlapping ancestor or descendant,
which may not be the only blocker. The snapshot may be stale; it does not prove
current ownership.
Success still waits for acquisition, and automatic mutators keep UI-only
diagnostics. Ordinary tool header/payload text and harness-injected provenance
envelopes are separate layers; producers do not author context envelopes.
An interrupted `wait` does not establish the operation's terminal outcome.

## Instructions and skills

Discovery stacks user `AGENTS.md`/`AGENTS.*.md` from
`$HOME/.config/agents/`, `$HOME/.config/agents.local/`, `$HOME/.agents/`, and
`$HOME/.agents.local/`, plus cwd ancestors and their `.agents.local/` files.
Skills come from project `.agents/skills`/`.agents.local/skills` and those four
user roots' `skills` directories. XDG user-skill names beat alternate-root
duplicates before modified-time collision resolution. `.local` roots suit
machine-specific files.

Agent initialization uses remembered/inherited/restored cwd. A persistent setter,
even to the same path, refreshes that instance's project discovery; getters and
call-local overrides do not. Dependent inference and `:skill` expansion wait for
installation. After tool results, the harness delivers each new or updated
instruction file in a separate `agents_md` envelope and groups catalog entries
in one `skills` envelope, without a separate turn. It compares each item with its
latest delivered content: changed or reverted items carry full content and
updated markers; unchanged returns and removals are silent. Scan failure removes
stale project input but preserves
user/other-source input; use an absolute or same-path setter to repair it.

Skill metadata supports `user-invocable`, `disable-model-invocation`, and
`argument-hint`. `disable-model-invocation` hides the skill from model discovery
and implies user invocation. `:skill <name> [args]` or `:skill:<name> [args]`
uses the selected agent's frozen discovery snapshot.

Skills and AGENTS files accept YAML lists `only-roles`, `only-role-groups`,
`except-roles`, `except-role-groups`. Allow dimensions form a union; exclusions
win. Missing allow lists admit everyone; explicit empty lists match nobody.
Names match actual roles/groups exactly. Winners are chosen before filtering;
hiding one never exposes a duplicate. Eligibility freezes at initialization or
project refresh, while later loads read current bodies. Malformed filters warn
and fail open. These filters are not ACLs. Discovery follows symlinks: trust the
project's instruction and skill files before running Tau there.

## Configuration

```json5
extensions: {
  "core-shell": {
    config: {
      working_directory: "/srv/project",
      shell: {
        command: "bash",
        prefix: ["nix", "develop", "-c"],
        allowlist: [
          { workdir: "/srv/project/**", command: "cargo *" },
          { workdir: "/srv/project", command: "jj status" },
          { workdir: "/srv/project/**", command_regex: "jj (?:log|show [a-z]{6,32})" },
        ],
        user_command_timeout_secs: 3600,
        non_interactive_pager: true,
        extra_env: { PATH: "/custom/bin:/usr/bin" },
      },
      dir_lock: { enable: false, backend: "memory", enforce_ro_bind: true },
    },
  },
}
```

`working_directory` sets startup cwd, never restored agent state; late changes
are rejected. Execution uses `<command> -c <user command>` after `shell.prefix`.
`extra_env` overrides inherited values; empty values remove variables. The pager
overlay applies afterward. `user_command_timeout_secs` affects user commands
only; tool calls use their own `timeout`.

`allowlist` is a guardrail, not a sandbox. Omission permits all commands; `[]`
denies all model/user shell commands. Each rule requires canonical absolute
`workdir` plus exactly one whole-string, case-sensitive `command` glob or Rust
`command_regex`. Both must match. Workdir `*` stays within one component; `**`
crosses components. Command globs treat separators/newlines as ordinary
characters. Regexes have implicit absolute anchors; omit `^...$`.
Case-insensitive flags are invalid.

Limits: 32 rules, 2,048 authored UTF-8 bytes per pattern, 262,144 compiled bytes
per matcher, optional `description` up to 1,024 bytes. Descriptions appear in
model guidance/denials: do not include secrets. Rules do not inspect shell syntax,
wrapper argv, environment, `PATH`, or resolved executables, and do not cover
in-process `grep` or unrelated subprocess systems.
