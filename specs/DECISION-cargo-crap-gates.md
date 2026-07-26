# DECISION-cargo-crap-gates: Lower cargo-crap gates through code quality

Authority: confirmed, 2026-06-23, user

## Decision

New or worsening CRAP failures are fixed by simplifying or decomposing code and by
adding meaningful tests, never by increasing limits or adding baseline exceptions.
The baseline exists only to prevent historical debt from blocking unrelated work
and should shrink.
