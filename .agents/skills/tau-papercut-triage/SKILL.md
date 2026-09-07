---
name: tau-papercut-triage
description: >
  Use when asked to "triage papercuts", review clanker-reported problems, or
  analyze and clear tau dev papercut reports. Distinguish agent mistakes,
  recurring environment issues, and Tau defects; queue actionable follow-ups
  for approval before clearing analyzed reports safely.
advertise: true
---

# Triage papercuts

Use a senior researcher for this task. Triage is investigation and reporting,
not permission to fix code, change configuration, restart sessions, or bypass
isolation. Respect any concurrent project update owner.

## Capture the complete inventory privately

1. Load `dpc`, `tau-self-knowledge-debugging`, and applicable project instructions.
   Read the papercut and relevant trust-boundary sections of `SECURITY.md`.
   Load `dpc-rustgrep` before structural Rust lookups, and `linked-specs` before
   investigating governed behavior through its specs.
2. Verify the installed interface and build rather than assuming flags:

   ```sh
   tau --version
   tau dev papercut --help
   tau dev papercut list --help
   tau dev papercut clear --help
   ```

3. Capture one complete authoritative CLI snapshot in an owner-private
   directory. Reports contain unredacted model text and may contain secrets.
   Do not dump raw reports into shared `/tmp/public`, tickets, or the conversation.

   ```sh
   umask 077
   private_dir=$(mktemp -d /tmp/tau-papercut-triage-XXXXXXXX)
   tau dev papercut list --markdown > "$private_dir/snapshot.md" &&
     sha256sum "$private_dir/snapshot.md" > "$private_dir/snapshot.sha256"
   ```

   Stop if listing fails; do not clear malformed/unreadable data. `--state-dir
   <STATE_DIR>` is supported on both `list` and `clear`; use the same explicitly
   selected root for both when overriding the default. This CLI covers the
   standard `std-utils` instance, not arbitrary reporter instances.

4. Give every report a stable ordinal within that snapshot and preserve its
   timestamp and attribution privately. Count actual report records, not arbitrary
   Markdown heading matches inside report bodies. If taking plain and Markdown
   snapshots separately, verify their record sequences agree: each list call
   takes its own snapshot and reports may arrive between them. Plain output
   escapes controls but is not a general structured interchange format.
   Record capture time, first/last report timestamp, count, snapshot hash, CLI
   build, and inspected source revision.

## Classify evidence, not reporter conclusions

Account for every report, including duplicates and multi-symptom reports.
Group repeated symptoms into findings but retain an ordinal-to-finding ledger.
Repeated reports from one incident are not independent reproductions.

For each finding record category, priority, confidence, recurrence, concrete
evidence, disposition, and the smallest useful follow-up:

* **Agent mistake / expected behavior:** usually low priority; ignore once
  explained. Record useful prevention improvements when mistakes recur or have
  meaningful consequences. A quoting mistake that runs a command or an overly
  broad cleanup is not harmless merely because the harness behaved correctly.
* **External environment / configured integration:** medium priority by default.
  Distinguish one-off recovery, recurring contention, stale paths/builds,
  missing tools, configured policy denials, and a fixable integration mismatch.
  Identify the responsible project or configuration rather than automatically
  filing a Tau core defect.
* **Tau harness/product defect or suspected defect:** highest investigative
  attention, with impact-based severity. Separate proven defects from
  observations still needing a minimal reproduction. Include misleading tool
  schemas/docs, inspection failures, and visibility defects, not only crashes.
* **Already fixed / stale / unresolved:** use these as dispositions, not as a
  substitute for a category. A source commit is not proof every live daemon
  runs the fix. State which target you actually rechecked.

Check exact submitted arguments and canonical tool results before attributing a
problem to Tau. Particularly useful counterchecks:

* Backticks in double-quoted shell prose execute substitutions. Check quoting
  before alleging command/result mixups. Use literal heredocs/files for prose.
* Look for earlier successful unlocks before concluding a manual lock vanished.
* Check human UI and peer-message ingress before calling a child's work unsolicited.
* Automatic tool backgrounding does not remove the command's explicit/default
  timeout. A completion notification is not necessarily a consumed result.
* A deduplication pointer means identical **output**, not skipped command execution.
* `jj` reads may snapshot/import mutable working-copy state. Pin immutable Git
  objects or use documented `jj --ignore-working-copy` when appropriate.
* Directory-scoped wrappers and direnv may write before a nominally read-only
  command. Avoid inspecting through a mutating wrapper when a neutral workdir
  and explicit target path suffice.
* Hidden runtime mounts can make discovery empty by design. Distinguish that
  from mixed-version exact-admission failure and real discovery bugs.
* Provider-hosted `web.run` failures are not automatically Tau registered-tool
  failures. Keep the documented provider/native boundary clear.

Use bounded local source/history and diagnostic inspection. Durable agent logs
are canonical replay evidence; session debug JSONL is incomplete, so a missing
debug row does not prove an event never happened. Raw provider captures, tool
arguments, and logs stay private. Do not run mutating probes, broad tests, or
restoration operations without separate authority. Consult the relevant
persistence/interface gates before proposing semantic changes; do not expand
this task into hostile local-IPC hardening.

## Report and queue before clear

Produce a sanitized report with:

1. Main findings and category/priority counts, with the scope of uncertainty.
2. Deduplicated findings and actionable scopes, including stale fixes and
   harmless explanations.
3. A complete per-report coverage ledger and the exact reviewed snapshot cut.
4. Reproduction/evidence references without raw secrets.
5. Clear status and any late-arrival or access blocker.

Write actionable follow-ups as discrete project task/ticket entries on the
**active work queue, initially blocked pending the user's review and approval**.
State proposed scope, evidence/confidence, priority, and approval gate in each
entry. Use the project's actual ticket/queue workflow; when delegated, send
entries to the coordinator who owns those writes rather than duplicating them.
Record useful agent-prevention and environment work too, not only suspected
harness bugs.

Only under an explicit user delegation may the coordinator approve an **obvious
Tau bug with an obvious narrow fix**. Record the delegation, exact scope, and
reason it qualifies; retain normal engineering review and CI. Non-obvious,
uncertain, environmental, or meaningful semantic work stays user-gated. This
exception is not blanket permission to auto-implement or dispatch triage
findings. Ask the coordinator to acknowledge durable recording before clearing.

Shared reports may use an unpredictable `mktemp
/tmp/public/papercut-triage-XXXXXXXX.md` path, but `/tmp` alone is not durable
tracking: preserve the sanitized findings and coverage in the project's
ticket/report workflow before removal of the source reports.

## Clear only through supported preservation semantics

Recheck the installed clear behavior before acting. In the current CLI, `list`
is lock-consistent and `clear` takes the same cross-process append lock, but
deletes **every record present at its serialized clear boundary**. It reports
`cleared N papercut report(s)`; appends admitted after that boundary remain.
The interface has no ID selection, snapshot token, `--before`, `--dry-run`, or
compare-and-clear option.

While that delete-all behavior remains, a final re-list followed by clear is
**not atomic**: a stable count, timestamp, or hash cannot rule out an
unreviewed arrival between the two commands. Do not clear unless an explicitly
supported quiet period covers all reporters sharing the selected state root.
Otherwise report **analysis complete, clear deferred**, preserve the private
snapshot and durable report, and do not invent store locks or direct
rename/truncate/delete workarounds.

1. Confirm clearing is authorized and the report/queue entries are durable.
   If the original user request already authorized clear after triage, do not
   ask for duplicate approval; coordinate the execution boundary with the
   parent/coordinator.
2. Under a confirmed quiet period, capture and compare the final complete record
   sequence, then analyze/report/queue every late arrival before running:

   ```sh
   tau dev papercut clear &&
     tau dev papercut list --markdown > "$private_dir/post-clear.md" &&
     sha256sum "$private_dir/post-clear.md" > "$private_dir/post-clear.sha256"
   ```

   Add the same `--state-dir <STATE_DIR>` to both if a non-default root was used.
   Record the exact clear count and a sanitized post-clear status. Keep any
   remaining report text and attribution private. New post-clear reports are not
   evidence that clearing failed; do not blindly clear again.
3. If a later installed CLI provides a supported archive/preservation clear
   operation, follow its actual documented semantics instead. Verify its
   preservation and append boundary against the current implementation and
   ask the coordinator to arrange any needed skill update; never emulate it by
   manipulating the reporter store directly.

Finish with a concise summary of what was analyzed, what deserves user review,
which reports were cleared (or why none were), and where the durable report
and approval-gated or explicitly delegated follow-ups live.
