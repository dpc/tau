# DECISION-no-backward-compatibility: No backward compatibility for internal formats

Authority: confirmed, 2026-07-18, dpc

## Decision

Tau provides no backward-compatibility support or migrations for its internal
serialized protocols and persisted structured data. These formats may contain
numeric version fields, but their values remain `0` unless this confirmed
decision changes. Cargo, package, and release versions are outside this policy.

Tau's immaturity makes compatibility machinery an unjustified ongoing cost; the
tradeoff is that internal data and peers may break across revisions.
