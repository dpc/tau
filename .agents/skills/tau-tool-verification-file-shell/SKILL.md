---
name: tau-tool-verification-file-shell
description: >
  Use this skill when verifying Tau file and command tools: read, edit, replace, apply_patch, shell, or shell_command, including ranges, UTF-8, truncation, diffs, timeouts, mutation safety, and shell lock coverage.
advertise: false
---

# Tau Tool Verification File Shell

Load `tau-tool-verification` first for the shared output structure, escaping,
line handling, tool-description, availability, and reporting guidelines.
This skill supplies the focused verification guidance for this tool group.

### Tool-specific guidelines

When verifying generic `shell`, test an explicit call-level `cwd` followed by an
omitted-`cwd` call. For model-visible `shell_command`, use `workdir` and then
omit `workdir`; its schema must not advertise or accept legacy `cwd`. The first
call must execute in its override, while the second must use the unchanged
per-instance persistent workdir. No agent metadata mutation or persistent
workdir notice may be emitted. Do not confuse `shell_command.workdir` with the
separate top-level persistent `workdir(path)` tool, and do not assume sibling
calls in one batch are causally ordered.

When `extensions.<instance>.config.shell.allowlist` is present, verify paired
rule behavior rather than testing command patterns in isolation. One rule must
match both the canonical absolute effective cwd and raw submitted command. Each
rule requires exactly one matcher: `command` retains the globset glob grammar,
while `command_regex` uses a case-sensitive Rust regex with implicit absolute
whole-string anchors. Confirm that denial through `shell`, `shell_command`, `!`,
and `!!` shows the typed configured matcher (`command_glob` or `command_regex`)
with its paired workdir and does not execute the command. If a rule has an optional
`description`, verify the prompt and all four denial surfaces show its exact
JSON-escaped trusted prose; prompt braces must additionally render as `\u007b` and
`\u007d`. Use a unique denied-command sentinel and verify generated `ToolError.message`
and `!`/`!!` denial output never contain it. Descriptions are limited to 1,024
authored UTF-8 bytes, do not affect matching, and must not contain secrets because
prompts and denials disclose them. An absent allowlist
remains unrestricted; an empty list denies all. Treat this as a best-effort
guardrail, not a sandbox test.

`read_image` accepts exactly one PNG, JPEG, or WebP path, an optional
`mode: high | overview`, and an optional `{x,y,width,height}` region; it has no
multi-image or original-detail form. Bare calls and explicit high calls must
retain the 2048-side/2,500-patch profile. Experimental overview must stay within
1024 pixels on either side and 600 rounded-up 32-by-32 patches. Regions use
half-open coordinates in the EXIF-oriented source, reject zero, overflow, and
out-of-bounds extents, crop before mode resizing, and report the exact
source/oriented/region/output geometry, profile, patches, and canonical byte
count. Verify that the tool sniffs and fully decodes bytes rather than trusting
the extension, rejects animated or oversized inputs, and returns one typed image
part paired with the original tool call. Both local profiles remain provider
high detail. Provider-visible base64 must exist only inside a native Responses
`input_image` data URL, never ordinary tool text or a synthetic user message.
Generic UI/debug output must expose metadata only. If the active route lacks
explicit image input and image tool-result modalities, the tool should be
absent.

The output of `read` and `shell` is intentionally similar, and should support
the same semantics. The meaning of the line prefix is different: line number vs stdout/stderr information

`read` supports either one top-level inclusive `start_line`/`end_line` range or a `ranges` array of up to 100 inclusive `{ start_line, end_line }` objects. Multi-range output uses the same line-number prefixes as normal read output, with exactly one empty line between requested chunks; overlapping ranges are allowed and return redundant chunks. Verify that mixed `ranges` plus top-level range arguments are rejected. `read` renders only actual file lines: `start_line` past the last actual line is an error, except `start_line: 1` is allowed for an empty file and returns empty content with zero totals. An `end_line` past EOF returns the available suffix rather than an error.
`shell` tool will add `duration_seconds: {number}` header for commands that took longer
than 5s to execute. Whole-second precision is acceptable; finer precision is
not needed. Reported durations are approximate, and can include overheads and
latencies of internal components.

Mutating ext-shell tools that wait more than 5s to acquire an automatic directory update lock should add `lock_wait_duration_seconds: {number}` to the final result or error details. Use the same approximate whole-second semantics as `duration_seconds`. Omit this header for waits of 5s or less, for tools that did not wait, and for canceled or abandoned waiters that never acquired the lock.

`shell` tool should return non-zero exits and timeouts as structured command
results with output details, not as tool invocation errors. It should reliably
timeout operations that take longer than timeout argument, but currently 100%
reliable child process termination is not implemented and will require advanced
techniques to implement in the future (e.g. cgroups).
When the model omits `timeout`, both `shell` and `shell_command` use a
300-second timeout; an explicit non-negative timeout remains call-local.

On Linux, Android, and macOS, ext-shell shell commands use independent PTYs for stdout
and stderr while stdin remains closed. Verify `[ ! -t 0 ]`, `[ -t 1 ]`, and
`[ -t 2 ]`; verify stdout and stderr remain separately prefixed in captured
output; and verify a poll/select-driven input consumer sees persistent readiness
rather than hanging.
Also re-check timeout,
cancellation, signal, background-descendant, invalid-UTF-8, line-ending, and
output-bound behavior through the PTY path. Terminal output newline translation
must not rewrite the command's original bytes. Other implementations retain
their platform pipe behavior.

For both model shell calls and user `!`/`!!` commands, verify the default
protected overlay wins over inherited values and `shell.extra_env` by exposing
`PAGER=cat`, `GIT_PAGER=cat`, `GH_PAGER=cat`, `JJ_PAGER=cat`, and
`SYSTEMD_PAGER=cat`. Verify it preserves `TERM` and leaves `MANPAGER` and
`BAT_PAGER` ordinary.

Run every focused internal regression:

```console
cargo nextest run -p dpc-tau-ext-shell -E 'test(non_interactive_pager_overlay_has_final_precedence_and_narrow_scope) or test(shell_isolation_preserves_inherited_term_by_default) or test(model_and_user_shells_share_protected_pager_environment) or test(protected_pager)'
```

Then verify the live surfaces. Configure every listed variable to a distinct
hostile value under `extensions.core-shell.config.shell.extra_env`, configure
`TERM: tau-verification-term`, restart Tau, and run this exact command once
through an exposed model `shell` / `shell_command` and once as user
`!! <command>`:

```sh
printf '%s|%s|%s|%s|%s|%s|%s|%s\n' \
  "$PAGER" "$GIT_PAGER" "$GH_PAGER" "$JJ_PAGER" "$SYSTEMD_PAGER" \
  "$TERM" "$MANPAGER" "$BAT_PAGER"
```

Both surfaces must print five `cat` values, the configured TERM, and the two
ordinary tool-specific values in that order.

To verify the explicit opt-out, copy
`crates/tau-ext-shell/tests/fixtures/hostile-pager.sh` to an executable temporary
path, set `non_interactive_pager: false`, set `PAGER` to that path, and set
`user_command_timeout_secs: 2`. After restart, `printf payload | "$PAGER"` must
reach the fixture and time out through both a two-second model call and user
`!!`; with protection enabled it must instead complete through `cat`. The
fixture consumes EOF and then stalls, so this procedure does not rely on host
pager configuration.

The protected `cat` must resolve on the child's effective `PATH`; otherwise an
ordinary command-not-found failure is expected.

#### Shell mutation and directory-lock mode

Older Tau versions exposed explicit `shell` `mode: ro` / `mode: rw` arguments.
Current ext-shell derives shell read/write behavior from manual `dir_lock`
coverage, not from command-content mutation detection. A shell command is treated
as read-only unless the caller already holds a matching manual directory update
lock; under that same-owner manual lock it is covered as a read/write command.
Do not expect a `mode` argument unless it is present in the live tool schema.

When verifying ext-shell `shell`, check both sides of that rule: shell commands
without same-owner manual-lock coverage bypass conflicting update locks (and use
read-only bind enforcement when available), while shell commands run by the
owner under a matching manual `dir_lock` are covered by that lock and keep it
active. When only provider/native `shell_command` is available, report that
ext-shell directory-lock and historical `mode: ro` checks are not applicable to
that tool schema.

Both non-Codex editor implementations are provider-visible as `edit`, so first identify the advertised schema. Ordinary models use the exact-text internal `replace` implementation described below. An explicit `shell:tool-style:edit` tag or `tool_policy.default_shell_tool_style: edit` selects the legacy line-coordinate internal `edit` implementation, also as provider-visible `edit`.

The line-coordinate schema selected by either mechanism requires each edit entry to include `start_line`, `end_line_exclusive`, `newText`, and `context_line`; it replaces the original half-open line range `start_line..end_line_exclusive` with `newText` as whole replacement lines. Empty insertion ranges use `start_line == end_line_exclusive`; non-empty replacements cover `start_line` through `end_line_exclusive - 1`. To replace read output lines A through B, use `start_line: A` and `end_line_exclusive: B + 1`. All edit ranges use the original file numbering as if applied simultaneously, so the tool must reject overlapping ranges before changing the file. Unlike `read`, `edit` must not clip ranges: both line slots must be at most `total_lines + 1`, and `end_line_exclusive` must be at least `start_line`. When non-empty `newText` lacks a trailing line ending, `edit` normalizes it into a full line using surrounding/replaced content as needed; explicit line endings in `newText` are preserved, so mixed line endings are allowed.
That line-coordinate editor supports file creation: missing files are treated as empty, and missing parent directories are created only after the request validates. To create a file, insert with `start_line: 1`, `end_line_exclusive: 1`, and an empty `context_line`. The model-visible result should stay minimal: `edits`, `changed`, `new_max_valid_start_line`, and `total_bytes`; `new_max_valid_start_line` is after-edit state and must not be confused with original range validation.

The line-coordinate editor requires a per-entry `context_line` string matching the original line immediately before `start_line`, excluding any line ending. Use an empty `context_line` when `start_line` is 1. EOF appends to a non-empty file must use the original last line as `context_line`; empty/missing-file creation uses an empty `context_line`. For ease of agent use, trailing literal `\r` and `\n` characters in the supplied context line are accepted and trimmed before matching; embedded `\r` or `\n` characters remain malformed. A malformed context line, missing context line, or context-line mismatch must leave the file unchanged. A mismatch returns read-like `line-numbered content` details around the expected context line plus up to 10 existing lines before and after it, with invalid UTF-8 and truncation handled like `read`; the BOF virtual context line is not rendered as a fake numbered line and is reported as `context_line_number: 0`.
The line-coordinate editor allows at most 100 edit entries per call. Requests with more entries must error out immediately before reading, writing, or creating parent directories. Invalid ranges, overlapping ranges, missing `newText`, missing or malformed `context_line`, malformed line fields, and context-line mismatches must leave the file unchanged. Error details should not echo raw edit requests; only purpose-built recovery details such as context-line mismatch context should be included.

For ordinary models, provider-visible `edit` is the internal exact-text
`replace` implementation and accepts exactly `{path, edits:[{oldText,newText}]}`
for one existing UTF-8 file and at most 100 entries. Verify it rejects unknown fields, empty
`oldText`, invalid UTF-8, duplicate/nonmatching/overlapping targets, and files
over 10 MiB without writing. It must match all targets in one original snapshot,
after CRLF/CR-to-LF normalization and ignoring only an initial UTF-8 BOM; do not
accept fuzzy Unicode, whitespace, punctuation, legacy aliases, or JSON-string
preprocessing. Verify BOM and untouched mixed-ending bytes survive, inserted
newlines use local source endings with LF fallback, success exposes only
`edits`, `changed`, and `total_bytes`, and changed UTF-8 files attach the normal
structured diff while no-ops attach none. With directory locking enabled it must
wait on the same automatic update lock class as the line-coordinate
implementation. Verify the provider definition/call/result names are `edit`
while ext-shell lifecycle started/result events retain `replace`. The explicit
`shell:tool-style:edit` selector uses the line-coordinate schema above.
`apply_patch` should follow safe patch semantics rather than shell `patch` clobber semantics. Verify `Add File` rejects an already-existing destination and preserves its original content. Verify move hunks reject moves whose destination already exists, preserving both the source and the existing destination; a move is distinct from an explicit update/delete and must not silently clobber another file. Context, line, hunk, add, delete, and move validation failures should not silently mutate unrelated content, and all file changes that are applied before a later partial failure must be reported clearly.

`apply_patch` output should stay compact and should not echo the full patch text back to the agent. For UTF-8 files it mutates, the tool should attach structured UI-only diffs; multi-file patches should attach one structured diff per changed file. When a later hunk fails after earlier hunks have already been applied, the agent-visible error must include structured partial-mutation details for the files/paths that changed where applicable, while the UI still receives diffs for those applied UTF-8 changes. Invalid UTF-8 or binary-like files should not produce misleading text diffs; report any missing, duplicated, or agent-visible raw diff payloads as tool-output regressions.

Context mismatches must keep `ToolError.message` single-line so the normalized
`error` header remains readable and injection-safe. Put the bounded,
path-labelled expected-context excerpt in `details.output`, where real
newlines are valid. Preserve escaped metadata paths, no-mutation evidence, and
any partial-change fields/UI diffs from earlier successful hunks.

For shell truncation, independently construct the expected rendered records.
`total_bytes` and saved artifacts count the complete UTF-8 rendering, including
`out` / `err` prefixes, flags such as `crlf`, `no_nl`, and `invalid-utf8`, plus
inserted separators. They deliberately do not report raw process payload bytes.

Other commands should adhere to pre-existing conventions and naming used in
standard tools.
