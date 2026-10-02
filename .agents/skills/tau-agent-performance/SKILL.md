---
name: tau-agent-performance
description: Generate routine trailing-two-week provider/model latency and output-throughput CSV and SVG charts from content-free durable agent traces.
---

# Routine agent performance charts

Run the checked-in helper; do not reconstruct timing from captures or research
the journal each time. It uses Python's standard library and Tau's built-in
validated `agent-performance-jsonl` trace projection. It never contacts a
provider or running harness.

```sh
cd "$(jj workspace root)"
python3 .agents/skills/tau-agent-performance/chart_performance.py \
  --out tmp/agent-performance-last14days
```

The output directory must be new. Keep `performance.csv`, `latency.svg`,
`throughput.svg`, `summary.txt`, and `README.md` together; do not commit them.
The helper prints the summary and artifact path. It creates an owner-only
directory, but the artifacts still reveal aggregate activity and model names.
Inspect them before sharing.

## Presentation defaults

Deliver readable **PNG previews of both charts**, with the CSV, styled SVGs,
and coverage summary available alongside them. The helper currently produces
basic SVGs; it does **not** implement the styling below. Restyle from its CSV
in a temporary script as needed, preserving the measurements and bucket gaps.
Keep that script with the private artifacts for reproducibility.

* Build one exact-model color map and reuse it for latency and throughput,
  across all accounts. Give model families stable hues and versions
  distinguishable shades: **Astra red, Sol green, Luna blue, Terra yellow**.
  Starting palette: Astra 6 `#dc2626`; Sol 5.6 `#15803d`, Sol 6 `#65a30d`,
  Sol 6.1 `#166534`; Luna 5.6 `#1e40af`, Luna 6 `#0284c7`;
  Terra 5.6 `#a16207` (dark golden yellow for readability on white);
  Grok `#be185d`; Qwen `#7c3aed`.
  Bind these colors to the actual recorded model strings. For new versions,
  extend the family's shades; for new families, choose a distinct hue.
  Family styling is presentation only: keep exact model identities and
  measurements separate, without guessing aliases or merging versions.
* Use account marker shapes consistently: `chatgpt` circle, `chatgpt-fedi`
  square, `grok` diamond, `ren` triangle. Assign other recorded accounts
  distinct shapes. The CSV's `provider` is the recorded series key, not proof
  of account identity; use account labels only when that mapping is known,
  otherwise label the shape legend **Provider**.
* Use two compact legends: **Model** with colored samples and exact model
  names, and **Account** (or **Provider**) with neutral marker shapes. Order
  model families **Astra → Sol → Terra → Luna**, then other families in stable
  alphabetical order; keep versions in stable ascending order within each
  family (compare numeric version components numerically). Reuse this order
  in both charts. Reserve
  enough space to keep legends and labels readable without covering data.
* Prefer logarithmic y axes when positive values span a wide range. Label
  latency **seconds (log scale)** and throughput **tokens/wall-second
  (log scale)** when using log scales. Choose ticks and limits from the data,
  not fixed ranges that clip observations. Keep genuine zero rates visible
  using a linear scale or a clearly labeled separate zero indication; never
  turn missing values into zeros or substitute an epsilon on a log axis.
* Show the UTC range and six-hour median bucketing. Preserve gaps, clipped
  boundary buckets, and today's current partial bucket. Include a short
  wall-time caveat and any material skipped-journal or sparse-coverage caveat
  in the delivery, using `summary.txt`.

Render styled SVGs to PNG at about 1600 pixels wide (for example,
`rsvg-convert -w 1600 -o latency.png latency.svg`, then likewise for
throughput). Inspect both PNGs at delivery size before sharing: confirm
legible labels, unclipped legends, consistent colors/shapes, correct units,
and honest zero/missing handling. Export the inspected PNGs through the
artifact tool for inline previews; make the source artifacts available too.
If no supported image viewer is available, report that visual inspection
could not be completed rather than claiming it passed.

**“Last two weeks” ends at the current moment, not the last midnight.** The
default captures UTC now once before discovery/scanning, then selects the
trailing fourteen days `[since, until)`. Today's current partial six-hour
bucket is included. The companion `tau-qodq` quota/token workflow uses the
same current-moment convention and clips token-rate denominators at the range
boundaries. Neither workflow should round the endpoint down to midnight.

For reproducible comparisons:

```sh
python3 .agents/skills/tau-agent-performance/chart_performance.py \
  --agents-dir "$HOME/.local/state/tau/agents" \
  --since 2026-09-17T13:25:00Z --until 2026-10-01T13:25:00Z \
  --out tmp/agent-performance-fixed-range
```

Pass `--tau /path/to/tau` when the installed executable lacks current trace
support. The default root is `$XDG_STATE_HOME/tau/agents`, falling back to
`$HOME/.local/state/tau/agents`. Every immediate directory with `events.cbor`
is scanned once, root-only, through a finite validated snapshot. There is no
time index: a bounded output range does not reduce journal bytes scanned.
Large histories can take several minutes. `--timeout` limits each individual
trace command (default 120 seconds); it does not limit the full scan. The
maximum range is 366 days. The helper retains selected scalar samples for
medians and deduplication identities for the current journal, so memory scales
with selected completed prompts plus prompt identities in the largest scanned
journal, not raw trace bytes.
Temporary traces are anonymous private files and removed automatically.

## What the charts mean

Each line identifies the **exact recorded provider/model**, including version.
The first slash of the canonical model separates provider from model; aliases
such as sol/astra/terra/luna are not guessed or merged. Both charts use ordinary
completed inference prompts in UTC-aligned six-hour buckets, assigned by the
accepted terminal timestamp. The CSV reports completed, latency, and rate
sample counts separately for every bucket/model:

* **Latency:** median prompt-materialization to accepted response terminal
  journal-wall interval, in seconds.
* **Throughput:** median of each prompt's `response_received_tokens /
  prompt-to-terminal elapsed seconds`. This is **not pure decoder speed**.
  Prompt preparation, provider work, retries, and harness overhead can be in
  the denominator. Retries are not separate chart samples.

These are rough aggregate comparisons, not a wire profiler. First-output
timing and separate harness overhead are unavailable in this durable projection.
Standalone compaction is excluded. Missing terminals, missing timestamps,
decreasing clocks, and missing usage are not zero measurements. Present zero
tokens with positive elapsed time is a real zero-rate sample; zero elapsed
time cannot supply a rate.

Points sit at each clipped bucket's midpoint. Only adjacent buckets with that
metric connect; missing buckets are gaps, not inactivity. Partial boundary
buckets use only selected prompt samples; these per-prompt medians do not
divide by bucket duration. No observations are fabricated at “now.”

Check `summary.txt` before comparing providers: it reports scanned/successful,
failed/unsupported/timed-out journals and selected/missing samples. Old schema
journals can be unsupported; running agents can lack checkpoints or have a
checkpoint behind their newest activity. Those journals are skipped explicitly,
not treated as zero performance. Incomplete and missing-terminal-time counts
cover all scanned history because their completion time is unavailable.
Different workloads, reasoning effort, tool use, cache state, and retry rates
can dominate model differences. A sparse median is only sparse evidence.

## Maintenance

Reuse `docs/agent-trace.md` for the scalar schema and timing fidelity. Do not
add provider captures, prompts, errors, or agent IDs to exported artifacts.
The canonical trace projector owns typed correlation and journal validation.

```sh
python3 .agents/skills/tau-agent-performance/test_chart_performance.py
python3 .agents/skills/tau-qodq/test_extract_quota.py
```
