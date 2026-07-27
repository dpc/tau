# GATE-no-backward-compatibility: No backward compatibility for internal formats

## Gate

Tau must not add backward-compatibility support or migrations for internal
serialized protocols or persisted structured data. Their numeric version fields
must remain `0`.

## Justification

The user wants to avoid compatibility machinery while Tau remains immature and
accepts that internal data and peers may break across revisions. Cargo, package,
and release versions are outside this constraint.
