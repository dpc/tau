# DECISION-no-backward-compatibility: No backward compatibility for internal formats

Authority: confirmed, 2026-07-18, dpc

## Decision

Tau provides no backward-compatibility support or migrations for its internal
serialized protocols and persisted structured data. These formats may contain
numeric version fields, but their values remain `0` and are never incremented
unless a user or maintainer explicitly changes this confirmed decision.

Cargo, workspace, package, and release versions are outside this policy.

## Rationale

Tau is very immature. Compatibility and migration machinery would add ongoing
cost and complexity without a current benefit.
