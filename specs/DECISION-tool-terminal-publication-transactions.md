# DECISION-tool-terminal-publication-transactions: Tool terminal append authority

Authority: confirmed, 2026-07-23, dpc

## Decision

For each journal-backed, transcript-owned foreground tool call, the existing
durable provider-terminal journal record is the sole completion authority.
Nothing that assumes the terminal exists may advance before its semantic append
commits. Renderer-facing output is derived and delivered afterward; renderer and
provider publications are not one transaction.

Foreground open, lock, write, or unrecoverable rollback failure prevents that
semantic append. Background sync failure does not fail-stop the journal or live
epoch. Locked recovery rebuilds the longest valid prefix, truncates its invalid
suffix, and must not
automatically resend uncertain tool, provider, or compaction effects.

## Rationale

One semantic append already provides the atomic completion point. Treating its
provider and renderer views as a publication transaction adds global retry and
lifecycle coordination without improving that authority boundary.

This decision is governed by
[DECISION-persistence-and-extension-interface-change-approval](DECISION-persistence-and-extension-interface-change-approval.md)
and constrains
[SPEC-terminal-tool-reports-and-canonical-outcomes](SPEC-terminal-tool-reports-and-canonical-outcomes.md).
The physical-storage failure clause is superseded by
[DECISION-semantic-journal-writeback-durability](DECISION-semantic-journal-writeback-durability.md).
