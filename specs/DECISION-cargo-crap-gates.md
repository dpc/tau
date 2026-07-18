# DECISION-cargo-crap-gates: Lower cargo-crap gates through code quality

Authority: confirmed, 2026-06-23, user

New or worsening CRAP failures are fixed by simplifying or decomposing code and by
adding meaningful tests, never by increasing limits or adding baseline exceptions.
The baseline exists only to prevent historical debt from blocking unrelated work
and should shrink.

Tau keeps shared non-CI-specific cargo-crap defaults in the root
`.cargo-crap.toml`. CI supplies only run-specific paths, workspace selection, and
job-specific output or threshold overrides, so local and CI policy do not drift.
