# DECISION-tool-terminal-publication-transactions: Tool terminal append authority

Authority: confirmed, 2026-07-23, dpc

## Decision

For each journal-backed, transcript-owned foreground tool call, the existing
durable provider-terminal journal record is the sole completion authority.
Nothing that assumes the terminal exists, including tracking cleanup, wait or
loop settlement, foreground completion or backgrounding, delegate teardown, and
next-inference eligibility, may advance before that record's semantic append
commits. Interception parking remains precommit. Renderer-facing output is
derived and delivered only after the authoritative append succeeds; renderer
and provider publications are not one transaction. Calls without this existing
durable transcript authority remain outside this decision's scope.

If the authoritative append fails because of physical storage or
persisted-journal integrity, the affected journal and its live session/harness
epoch must enter an explicit storage-faulted, fail-stop state and reject further
semantic work until reopen or restart. Pre-write validation, encoding,
size-limit, identity, and persistence-mode rejection does not latch the
fail-stop. The harness must not retain any exact rejected terminal envelope or
terminal-dependent continuation for online resumption, retry the terminal from
any global or per-call slot, or continue through the broken store. Reopen or
restart rebuilds from the committed prefix. Cold recovery remains conservative
and must not automatically resend uncertain tool, provider, or compaction
effects.

## Rationale

One semantic append already provides the atomic completion point. Treating its
provider and renderer views as a publication transaction adds global retry and
lifecycle coordination without improving that authority boundary.

This decision is governed by
[DECISION-persistence-and-extension-interface-change-approval](DECISION-persistence-and-extension-interface-change-approval.md)
and constrains
[SPEC-terminal-tool-reports-and-canonical-outcomes](SPEC-terminal-tool-reports-and-canonical-outcomes.md).
