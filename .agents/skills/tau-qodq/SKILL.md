---
name: tau-qodq
description: >
  Extract and chart canonical provider quota observations and terminal token usage
  as offline, privacy-aware CSV, SVG, and summary artifacts.
---

# Tau QODQ: offline quota and token-usage diagnostics

Use `extract_quota.py` for bounded historical diagnostics from canonical Tau
events. It uses only Python's standard library and writes redacted CSV, SVG,
summary, and artifact README files. It is an offline aid, not a Tau command;
do not change provider, journal, or runtime semantics for it.

## Run

Select each configured subscription explicitly. `LABEL` is presentation-only;
`PROVIDER` exactly selects canonical quota `payload.provider` and the
`PROVIDER/` prefix of canonical token-usage `usage.model`. This checked-in
extractor invocation is the complete reproducible generator template; change
only its bounded UTC range and output directory.

```bash
cd "$(jj workspace root)"
skill_dir=.agents/skills/tau-qodq
"$skill_dir/extract_quota.py" \
  --sessions-root "$HOME/.local/state/tau/sessions" \
  --profile chatgpt=chatgpt \
  --profile chatgpt-fedi=chatgpt-fedi \
  --since 2026-08-07T00:00:00Z --until 2026-08-21T00:00:00Z \
  --out /tmp/public/tau-qodq-chatgpt-chatgpt-fedi
```

The exact range is `[since, until)` in UTC and must start/end on UTC-day
boundaries, so token-chart buckets align at 00:00/06:00/12:00/18:00 and both
charts can guide every UTC midnight. The default is the fourteen complete UTC
days ending at the current UTC midnight.
Ranges over 366 days are rejected. The
compatibility `--provider NAME` selection remains equivalent to
`--profile NAME=NAME`; prefer repeatable `--profile`.

Keep the generated `README.md`, `summary.txt`, `quota.csv`, `quota.svg`,
`tokens.csv`, and `tokens.svg` together. Do not commit artifacts or session
data. The extractor scans selected `events.jsonl` files without an index, so
time bounds constrain output but may not reduce bytes scanned.

## Inputs and privacy boundary

Select only these exact nested canonical published events:

* `harness.provider_quota_changed` provides accepted full quota snapshots.
  Do not substitute provider `_reported` quota events.
* `provider.response_finished` provides accepted terminal
  `usage.model`, `prompt_sent_tokens`, `prompt_cached_tokens`, and
  `response_received_tokens`.

The extractor reads no provider capture files and never exports credentials,
prompts, response/output items, routes beyond the quota snapshot's normalized
route metadata, or raw event records. A canonical response terminal can contain
output items, but the extractor deliberately reads only the listed identity,
time, model-selection, and usage fields.

`provider.response_finished` does not carry a quota profile epoch. Token rows
therefore identify the selected configured provider/model prefix and human
label, not an account or credential. Quota rows retain `profile_epoch`, which
is opaque process-lifetime evidence, **not** an account identity. Never infer
that selected names, profile epochs, or separate sessions prove a shared or
different account.

The extractor structurally skips unselected JSON values before decoding any
terminal fields. In particular, it does not materialize `output_items`, error
details, prompts, or provider content while selecting terminal usage.

## Chart and CSV semantics

`quota.svg` shows one line for each selected subscription. It explicitly selects
the canonical default `codex/primary` series: the Codex adapter maps an official
nameless rate-limit observation to the canonical default `codex` pool, and
`primary` is the provider-normalized primary window. It retains the maximum actual
`remaining_percent` observation per subscription and UTC hour, breaking the line
for every missing hour. This display-only reduction never averages, interpolates,
predicts, or alters CSV evidence. Ties retain the latest `(observation time,
sequence, profile epoch)`.
`quota.csv` still retains every pool, window, and process epoch. The SVG legend
contains only the supplied subscription labels; it exposes no pool/window or
epoch IDs. Its values are `remaining_percent = 100 - used_basis_points / 100`.
It guides and labels every UTC day boundary.

`tokens.csv` retains selected canonical terminals in UTC-aligned half-open
one-hour rows `[HH:00, HH+1:00)`, selected by the terminal's
`recorded_at_micros`. `tokens.svg` reduces those rows to UTC-aligned six-hour
buckets starting at 00:00, 06:00, 12:00, and 18:00, with connected lines only
across consecutive buckets. It renders all three six-hour measurements on one
shared logarithmic `log1p` Y axis:

```text
Cache hits    = Σ prompt_cached_tokens / 21,600 tokens/s
Cache misses  = Σ (prompt_sent_tokens - prompt_cached_tokens) / 21,600 tokens/s
Output tokens = Σ response_received_tokens / 21,600 tokens/s
```

Subscription color identifies the selected profile; line style identifies the
metric. The chart contains exactly those six profile/metric lines. Its
zero-preserving transform is
`log(1 + six-hour tokens/s) / log(1 + largest displayed six-hour tokens/s)`: an
observed zero remains at the baseline, rather than being dropped or replaced
with a positive value. Y ticks label actual tokens/s values, not transformed
coordinates.

The SVG never invents a six-hour bucket for absent evidence and never connects
across a missing UTC six-hour bucket. Missing hourly rows are unknown/missing
evidence, not zero use. An absent
`usage` record is unavailable usage. An old canonical record lacking the
serialized `prompt_cached_tokens` field is also unavailable for this chart:
the extractor does **not** reinterpret it as Cache hits zero, and excludes
that terminal's three categories. A present zero is used because the current
canonical schema serializes the field as a non-optional count.

For token replay/catch-up deduplication, one terminal identity is
`(selected profile label, agent_id, agent_prompt_id, provider_attempt)`.
Repeated identities retain the earliest `(recorded_at_micros, selected file
path, line number)` record **before** the time filter; a replay inside a range
does not turn an original terminal outside it into new consumption. Conflicting
complete-usage counts are reported once per terminal identity, not summed. If
the retained earliest copy lacks `prompt_cached_tokens`, later explicit-zero
replays cannot replace it and that terminal remains omitted. This is intentionally separate from quota
plotting: quota is snapshot evidence, not additive consumption.

## Summary and interpretation

`summary.txt` reports selected profiles/files/bytes, candidate and validated
canonical events, malformed data, missing usage/cache fields, out-of-range and
unselected-model terminals, duplicate/conflicting token identities, retained
quota rows, omitted unchanged quota rows, hourly token rows, rendered values,
and elapsed time. Report these exact values, the exact selection/range, and
the artifact paths with any conclusion.

Remember:

* A quota plateau is repeated observed state, not continuous metering.
* A rise in remaining quota or reset shift can be reset/reconciliation, not
  negative consumption.
* Gaps, empty snapshots, missing terminal usage, and absent hourly rows are
  unknown, not zero.
* Token timestamps are canonical log-admission/accepted-terminal times, not
  provider metering instants.

Run the focused oracle after changing the generator:

```bash
python3 .agents/skills/tau-qodq/test_extract_quota.py
```
