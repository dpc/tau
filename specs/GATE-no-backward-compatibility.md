# GATE-no-backward-compatibility: No backward compatibility for internal formats

## Gate

Tau must not add backward-compatibility support or migrations for persisted
structured data, whose numeric physical-format version fields must remain `0`.
The shared harness-peer wire and extension-visible event boundary is the sole
exception: it uses an explicit independent `X.Y` protocol revision and rejects
an incompatible major revision without a legacy decoder or migration path.

## Justification

The user wants to avoid compatibility machinery while Tau remains immature and
accepts that internal persisted data may break across revisions. The protocol
revision provides best-effort skew detection for independently built extensions,
not backward-compatibility support. Cargo, package, release, and physical journal
versions remain independent of it.
