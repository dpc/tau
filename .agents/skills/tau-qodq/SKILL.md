---
name: tau-qodq
description: >
  Extract and chart Tau's canonical provider quota observations as an offline,
  privacy-aware CSV, SVG, and summary. Use for historical provider quota
  diagnostics without adding a Tau command or changing runtime semantics.
---

# Tau QODQ: Offline quota-observation diagnostics

Use `extract_quota.py` to answer a bounded historical question about provider
quota observations. It reads local event logs, writes CSV/SVG/summary artifacts,
and uses only the Python standard library. Default to one relevant session or a
small selected directory. Resolve the checked-in companion script from the
repository root as shown below. Treat all-local aggregation as diagnostic
evidence, not an account-level statement.

## Run

The default time range is the rolling 14 days ending at execution time. `--since` replaces
the lower boundary; `--until` supplies an exclusive upper boundary. Use UTC
RFC3339 timestamps or date values and record the chosen `[since, until)` range.
Ranges longer than 366 days are rejected so daily guides and artifacts remain
bounded.

```bash
# Run from the repository root so this checked-in companion path resolves.
cd "$(jj workspace root)"
skill_dir=.agents/skills/tau-qodq
"$skill_dir/extract_quota.py" \
  --sessions-root "$HOME/.local/state/tau/sessions" \
  --provider chatgpt \
  --out /tmp/public/tau-qodq

# Narrower, reproducible selection and range:
"$skill_dir/extract_quota.py" \
  --sessions-root /tmp/public/selected-sessions \
  --provider chatgpt \
  --since 2026-07-14T00:00:00Z --until 2026-07-28T00:00:00Z \
  --out /tmp/public/tau-qodq
```

Inspect `summary.txt`, retain it with `quota.csv` and `quota.svg`, and do not
commit generated artifacts or session data. The script scans `events.jsonl`
without an index: a date bound reduces output, not necessarily bytes read.

## Input contract and selection

Select **only** nested semantic events whose canonical name is exactly
`harness.provider_quota_changed`. Never select or combine
`provider.quota_replace_reported`, `provider.quota_patch_reported`, or
`provider.quota_clear_reported`: reported forms can be adjacent to the accepted
snapshot, and a committed report does not prove harness acceptance. The
canonical event is the protected, validated full current snapshot; it is also
emitted for late-subscriber catch-up.

The extractor enumerates exactly one `events.jsonl` beneath each selected session
directory (including selected session-directory symlinks), validates the outer
`published` record and nested canonical payload, and filters by configured
`payload.provider`. It never follows debug provider captures. Malformed canonical
candidates are counted; root enumeration and selected-file read failures stop the
run rather than silently producing an empty chart.

Rows are grouped by provider, profile epoch, limit ID, and window ID. Within each
group, consecutive observations with identical quota, reset, route, and timing
freshness evidence retain their first and latest points. The latest point records
the exact number of intervening rows in `omitted_unchanged_before`. Epoch changes,
zero-use reset changes, many-to-many route changes, timing-anchor changes, and
server-offset calibration changes remain evidence. Independent profile/process
epochs are never merged.

## Units and chart semantics

`used_basis_points` is the upstream normalized fraction used: `10000` means
`100%`. It is not tokens, credits, spend, or a count of quota units. The CSV
also gives `used_percent`, `remaining_basis_points`, and `remaining_percent`.
The latter two are derived exactly as `10000 - used_basis_points` and
`100% - used_basis_points / 100`; they are the displayed quantity.

The SVG is a **point-only scatter plot** of `remaining_percent`: it never
connects, interpolates, or predicts observations. Its x-axis is strictly
`usage_observed_at_unix_ms`, the provider observation time, rather than local
log admission time or reset time. Dashed vertical guides mark each 00:00 UTC
inside the selected `[since, until)` observation-time domain. Reset fields are
metadata and never set or expand the axis.
CSV collapse is evidence-preserving but lossy: it retains run endpoints and an
exact omitted count, but intermediate observation timestamps and sequences are
unavailable. To keep a large all-local SVG usable, rendering emits one circle
for observations that land on the same tenth-pixel coordinate and window color;
the summary distinguishes retained CSV evidence rows from rendered SVG circles.

## Artifact schema

`quota.csv` always uses one fixed schema, including when it has no rows. Copied
columns are provider, profile epoch, sequence, window identity, used basis points,
window duration, reset seconds, relative remaining seconds and timing anchor,
server offset and calibration time, and route evidence. `observed_at` and
`reset_at` are derived UTC renderings. `used_percent`, `remaining_basis_points`,
and `remaining_percent` are derived arithmetic values. Many-to-many route models,
provenances, and observation times are deterministic JSON arrays in their three
CSV columns. Empty reset/timing cells mean absent or unknown, not zero.
`omitted_unchanged_before` counts only rows actually omitted between the retained
first/latest evidence points. It does not reconstruct the omitted rows' exact
timestamps or sequences.

`summary.txt` reports exact scopes: selected canonical files and their logical
bytes; candidate lines containing the exact canonical-name token; validated
canonical events for the selected provider (including empty/out-of-range events);
malformed canonical candidates; sessions with in-range observations; in-range
window observations before collapse; emitted rows; and exactly omitted unchanged
rows. The extractor streams files and retains only normalized rows grouped by full
series identity; memory is proportional to selected canonical window observations,
never raw log size or unrelated/private event content.

Run the maintained focused fixtures after changing the extractor:

```bash
python3 .agents/skills/tau-qodq/test_extract_quota.py
```

Local/self CI runs the same command in its lint job.

## Evidence and interpretation

- A plateau means repeated observed state, not continuous metering.
- A rise in remaining quota or a reset shift can be a reset or reconciliation,
  not negative consumption.
- Gaps, empty snapshots, and absent rows are unknown/missing state, not zero
  use or zero remaining quota. Event logs are best effort and can have torn
  tails, loss, and restart gaps.
- `profile_epoch` is opaque lifetime/process evidence, not an account ID.
  A configured provider name and cross-session snapshots do not establish a
  common credential/account.
- Route bindings only describe an observed route for that snapshot. Blank
  values mean unknown, not all-model applicability. Bindings can be stale.
- `reset_at` is optional/server-declared. Relative timing requires its timing
  anchor; absolute reset interpretation needs fresh server-offset calibration.

Never inspect or export provider request captures for this task. They can hold
private prompts/outputs while the canonical quota event already contains the
normalized fields needed here. Report selection, elapsed time, bytes scanned, validated/malformed canonical
counts, emitted/omitted row counts, and these caveats with any conclusion.

## Scope and performance

For behavioral interpretation, create a directory containing only symlinks to
relevant session directories and pass it with `--sessions-root`. All-local
scans may be slow and can interleave unrelated harnesses/profiles. This skill
is intentionally an offline diagnostic: do not add a native Tau command and do
not change runtime, event, journal, or provider semantics to support it.
