# GATE-cargo-crap-gates: Lower cargo-crap gates through code quality

## Gate

The cargo-crap gate limits may not rise. Every production function measured by
the blocking gate must remain at or below the applicable limit. New or
worsening failures must be fixed through simpler code or meaningful tests.
Grandfathered above-limit exceptions may only shrink; once none remain, do not
recreate an exception baseline.

## Justification

The user wants complexity debt reduced rather than hidden. A baseline is only
appropriate as a temporary whitelist of pre-existing violations, not as an
inventory of functions already under the limit.
