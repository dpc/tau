---
name: tau-tool-verification
description: >
  Use this skill when asked to verify Tau harness tools or tool output behavior,
  especially read, edit, shell/shell_command, line-oriented output, truncation, metadata
  headers, UTF-8 handling, diffs, timeouts, or skill/tool conformance.
advertise: true
---

# Tau Tool Verification

Use when asked to verify Tau tool behavior or Tau tool-verification skills.

Tau exposes different tool sets depending on configuration, provider/model
capabilities, and extension setup. Common sets include:

* ext-shell's `read`, `read_image`, `edit`, and `shell` tools, plus related
  tools such as `dir_lock`; `read_image` appears only on explicitly
  image-capable provider routes;
* provider/native tools such as `apply_patch` and `shell_command`.

If not explicitly stated, start from the tools that are actually exposed in the
current session. For older/full ext-shell sessions, the default core set is
`read`, `edit`, and `shell`. For ordinary models, `edit` uses the exact-text
internal `replace` implementation; its provider definition, model calls, and
canonical results remain `edit`, while ext-shell started/result lifecycle
events use `replace`. The `shell:tool-style:edit` selector instead exposes the
legacy line-coordinate implementation as `edit`; ChatGPT/Codex uses
`apply_patch`. For provider/native sessions, map the same checks to
`shell_command` and `apply_patch` where possible, and explicitly report any
tool-specific checks that cannot be run because the corresponding tool is not
available.

## Goal

Your goal is to verify if basic Tau harness tools still work as expected,
and conform to our standards and guidelines.

## Guidelines

### Tool result output structure
All tools should return a normalized HTTP-protocol-like structure:

```
header-1: value-1
header-2: value-2
...
header-n: value-n

multi-line-payload
```

The canonical form is zero or more headers followed by an optional body. When
headers and a body are both present, one empty line separates them. A compact
body-only scalar response is valid and must not gain a leading empty line or
redundant status header.

`multi-line-payload` can be arbitrary, but line-oriented output typically uses
`<prefix>(optional-per-line-flags) <line-content>` structure. If that's the case
the tool description should mention it.

Tool outputs with non-trivial fields encoded into line-oriented
payloads should include a `format` header describing field order and names.
For example, an email listing can use:

```text
format: uid date from flags access attachments subject...

6212 2016-04-23T17:32:52Z builds@travis-ci.org seen,redacted preview 0 Hi there, from us
```

The `...` suffix on the last field in the format is used to indicate it is a multi-word field that extends to the end of the line.

Tool implementation must take care ensuring newlines and special characters are stripped from field values, and empty values use some placeholders (e.g. `-`) to avoid breaking the meaning of each line.

Many headers are optional, and skipped for their default most natural values
for token efficiency. Keep tool output compact: include only non-default,
non-redundant values that help the agent decide what to do next. Do not emit
aliases or duplicate fields that carry the same information.

Do not include headers that are straight copies of tool invocation arguments.
The calling agent already knows the arguments it sent, so echoing them wastes
context and makes the meaningful result harder to scan. Only report a requested
path, query, command, or similar argument when the tool has transformed it into
new information, such as a canonicalized path that differs from the input.

Harness-owned background-wait interruption is a successful control result, not
an ordinary completion. It uses closed `tau_internal: true`,
`wait_outcome: interrupted`, `wait_reason: activating_input`, and
`wait_mode: exact` or `any_background` headers. It does not echo a target ID or
consume the target result.

### Layered escaping policy

Tools must semantically escape untrusted metadata fields before composing
model-visible text. This includes paths, filenames, identifiers, owner names,
queries, commands when shown as metadata, and any other field whose bytes come
from the workspace, filesystem, user config, or an extension peer. Escaping is
local to the tool because the tool knows which substrings are metadata and which
substrings are user/file payload that should remain literal.

Line-oriented outputs must never let metadata inject extra records, headers, or
status lines. Escape at least `\\`, newlines, carriage returns, tabs, and other
control characters in metadata fields; use explicit flags such as `escaped` or
`invalid-utf8` when that helps the caller understand that displayed text is not
byte-exact. Do not over-escape file contents or command output just because they
are untrusted; those payloads have separate line-prefix, truncation, and UTF-8
handling rules.

Central provider-visible rendering should still apply a last-resort safety
invariant when structured tool responses are rendered into model input, but that
is defense-in-depth. It is not a substitute for tool-local semantic escaping,
because a central renderer cannot reliably know whether an arbitrary string is a
path, a header value, a status label, or content.

Terminal/UI sanitization is a separate layer. UI code must protect terminal
state and layout from control sequences, but UI escaping does not make
provider/model-visible text safe, and provider-visible escaping does not replace
terminal sanitization.

### Common patterns

`read` range operations use inclusive `start_line` and `end_line` fields. `edit` range operations use half-open `start_line` and `end_line_exclusive` fields.
Newlines are assumed to be `\n`, but other styles are supported
and displayed as `crlf` (`\r\n`), `cr` (`\r`) or `no_nl` (missing trailing newline).
This applies to both `read` line-number prefixes and `shell` stdout/stderr prefixes.

Lines containing invalid UTF-8 bytes should show Unicode replacement characters and an `invalid-utf8` flag,
so useful surrounding content remains visible while the agent knows the bytes were not exact.
Lines which are too long show a `truncated` flag and have content skipped.

Total outputs that are too long are truncated; `truncated: true`,
`total_lines: {lines}` and `total_bytes: {bytes}` headers are added.
These total headers are omitted when output is not truncated, except `read` may report `total_lines: 0` and `total_bytes: 0` for an empty file.
For shell output, `total_bytes` and saved artifacts count the complete rendered
UTF-8 shell form, including `out` / `err` prefixes, line-ending and UTF-8
markers, and inserted record separators. They are not raw stdout/stderr byte
counts.

When output is truncated due to line number limit, first and last 1000 lines
should be shown with `...` line separating them, instead of usual line prefix.
If a single line would exceed the visible byte budget for ext-shell `read`,
search/list/edit recovery, or user shell output (10 KiB), or model `shell` /
`shell_command` output (15 KiB), show only the native prefix plus `(truncated)` rather than
partial content.

Visible-cap-truncated ext-shell output, including `read`, `grep`, `find`, `ls`, edit
recovery, model shell, and user shell surfaces, must preserve native rendering
and include complete
`total_lines` and `total_bytes`, a compact warning to prefer narrower commands
or filters, and normally an exact temporary artifact path. Artifacts up to the 16 MiB
saved cap use `full_output_path`. Output beyond the saved cap must instead use
`saved_output_path`, `saved_output_truncated: true`, and `saved_output_bytes`;
it must never call that partial artifact full output. Verify the exact file is
readable while its random parent directory is private and not listable. Verify
privacy/filesystem failures instead emit `saved_output_unavailable: true`, then
verify ordinary cleanup only after both 32 later relevant calls and roughly 15
minutes, plus independent graceful shutdown and safe crash cleanup.
Caller-requested result limits and grep'"'"'s per-line shortening retain their
native limit metadata and do not by themselves imply a saved artifact.

For complete terminal-frame budgeting, verify an oversized `read_image` result
fails as typed content without base64/text fallback. For an oversized
`edit`/`apply_patch` structured diff, verify only the optional UI diff becomes
an explicit truncation marker while success or partial failure and changed-file
evidence remain truthful.

`grep` renders matches heading-grouped: each file's path appears once as a
heading line, followed by `LINE:CONTENT` for match lines and `LINE-CONTENT`
for context lines. Over-long path headings are truncated to the same
`GREP_MAX_LINE_LENGTH` (500 chars) as match body lines, with an ellipsis and
the "Some lines truncated to 500 chars" notice, so every rendered line stays
within the 500-char budget.


### Tool descriptions

Tool description should be short but informative. They should mention the line prefix meaning, if used in the tool. They should mention line and byte limits.


### Focused verification skills

Load the focused skill for every tool group in scope. The index contains shared
output rules; the focused skills contain the detailed tool-specific plans.

* `tau-tool-verification-file-shell` — `read`, both `edit` implementations
  (including the internal exact-text `replace` lifecycle name), `apply_patch`,
  `shell`, and `shell_command`, including ranges, UTF-8, truncation, diffs,
  timeouts, mutation safety, and shell lock coverage.
* `tau-tool-verification-background-cancel` — background tool completion,
  `wait`, `cancel`, and the required background/active-wait `agent_start`
  interruption probes, including consumption races, prompt suppression,
  delegate interruption, isolation, and event-log checks.
* `tau-tool-verification-directory-locks` — `dir_lock` conflict behavior,
  automatic lock scopes, lock wait metadata, cancellation, force unlock, and
  lifecycle cleanup.
* `tau-tool-verification-agent-coordination` — `message`, `agent_start`, and
  `agent_watch`, including routing, validation, queued and active-wait
  interruption, notification formatting, and deduplication. Always pair it
  with `tau-tool-verification-background-cancel` when verifying `agent_start`.
* `tau-tool-verification-status` — `status` transitions, validation, Working
  acknowledgement persistence, and activation steering around routine tool
  rounds and watched-agent events.

If a request spans groups, load all applicable focused skills. Apply the shared
guidelines in this index to every group, and explicitly report unavailable or
version-specific tools rather than silently skipping their checks.

### Verification procedure

Create a scratch directory in `/tmp` for your experiments and always avoid dangerous or disruptive actions during testing.

#### Model-visible parallel-call probe

Test parallel tool calling through the actual provider and harness, rather than
inferring support from a capability flag. In **one assistant message**, emit
four sibling calls to the available shell tool (`shell` or `shell_command`).
Do not use a batching/parallel-wrapper tool, and do not launch any of the four
calls from a later assistant turn: either would bypass the provider behavior
this probe is intended to test.

Use these four commands as the respective call arguments:

```sh
python3 -c 'import time; ident="parallel-1"; start=time.time_ns(); time.sleep(3); end=time.time_ns(); print(f"id={ident} start_ns={start} end_ns={end} elapsed_ms={(end-start)/1_000_000:.3f}")'
python3 -c 'import time; ident="parallel-2"; start=time.time_ns(); time.sleep(3); end=time.time_ns(); print(f"id={ident} start_ns={start} end_ns={end} elapsed_ms={(end-start)/1_000_000:.3f}")'
python3 -c 'import time; ident="parallel-3"; start=time.time_ns(); time.sleep(3); end=time.time_ns(); print(f"id={ident} start_ns={start} end_ns={end} elapsed_ms={(end-start)/1_000_000:.3f}")'
python3 -c 'import time; ident="parallel-4"; start=time.time_ns(); time.sleep(3); end=time.time_ns(); print(f"id={ident} start_ns={start} end_ns={end} elapsed_ms={(end-start)/1_000_000:.3f}")'
```

Before the probe, resolve the required interpreter in the effective execution
environment. Do this before changing persistent workdir, or use a verified
absolute executable path. If it is unavailable, report the probe unavailable
rather than treating command-not-found as a concurrency failure.

Confirm from the canonical `provider.response_finished` aggregate or an exact
provider capture that all four call IDs occurred in one provider terminal.
Visual adjacency in the UI or one apparent assistant message is insufficient:
calls emitted by separate provider responses do not test sibling scheduling.

Interpret the model-visible results by call identity, not by result-delivery
order. For each interval use `[start_ns, end_ns]`, and compute the overall
makespan as `max(end_ns) - min(start_ns)`. Normal process startup and scheduler
jitter mean starts and ends need not be exactly equal.

* **PASS:** all four results are present, each elapsed time is approximately
  three seconds, the intervals have a common overlap
  (`max(start_ns) < min(end_ns)`), and the makespan is normally about three to
  five seconds (use six seconds as a conservative upper bound).
* **FAIL — serialized execution:** all four calls were emitted together, but
  their approximately three-second intervals are sequential/non-overlapping
  and the makespan is approximately twelve seconds (ten seconds or more is a
  useful lower bound).
* **FAIL — provider emission:** the provider/model does not emit all four
  sibling calls in the one assistant message. Do not issue missing calls in
  later turns and misreport that as a parallel test.
* **INCONCLUSIVE:** report partial overlap, unexpected per-call duration,
  missing/malformed output, or a makespan between the pass and serialized
  bounds, then repeat the one-turn probe before assigning the failure to a
  layer.

When all four calls were visibly emitted in one assistant message but their
recorded execution intervals serialize, provider emission succeeded and the
evidence points to harness/extension scheduling. If the provider never emits
the four sibling calls, execution-layer concurrency was not tested. Report the
classification, four identity-tagged intervals and elapsed times, makespan,
overlap observation, and which layer the available evidence implicates.

For every tool thoroughly consider all corner cases, including ones which are not covered
in this document.

Negative probes intentionally produce tool failures, so do not run enough of
them consecutively to trip Tau's loop guard and then report the pivot as a tool
defect. Three identical failures or four consecutive distinct failures can
trigger the guard; one successful terminal resets the streak. Isolate negative
groups in short-lived delegates or insert a harmless successful tool call
between groups. A pivot below threshold, after a successful reset, or with
incorrect argument-sensitive grouping remains a discrepancy.

Report back:

* discrepancies between this document and actual usage,
* things that are wrong, confusing, inconsistent or unclear in both this document and actual tool output
* ideas for improvements both in the tool behavior and this document
