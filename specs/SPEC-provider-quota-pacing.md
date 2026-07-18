# SPEC-provider-quota-pacing: Provider quota pacing

Provider quota is ephemeral bounded current state, not per-response token usage,
retry policy, credits, spend control, or transcript state. Provider adapters
normalize upstream quota; the built-in provider reconciles full and sparse
updates; the harness validates source, bounds, epoch, and sequence before
exposing a full snapshot; the CLI computes generic pacing and presentation.
Protocol records contain no credential, account ID, provider display prose,
plan, balance, or raw response.

The governing choice is
[DECISION-provider-quota-pacing](DECISION-provider-quota-pacing.md).

## Acquisition and ownership

ChatGPT acquires a full account snapshot from authenticated `/wham/usage` and
reconciles supported sparse HTTP response headers and WebSocket
`codex.rate_limits` events. The provider adapter owns upstream parsing and
normalization. The built-in provider extension owns credentials, profile epochs,
full-fetch coalescing, and sparse/full merging. The harness verifies that the
sending connection owns the named provider, validates bounds and strict
epoch/sequence order, and exposes only a complete current snapshot.

## Applicability

A colored claim requires a fresh explicit `ModelId` to quota-pool binding from
an in-band route observation. A full account snapshot, a display name, or the
presence of only one pool is never applicability evidence. In the official
WebSocket contract, a valid `codex.rate_limits` turn event with neither
`metered_limit_name` nor `limit_name` authoritatively denotes the default
`codex` pool; this exact-turn signal is distinct from inferring applicability
from account pool presence. JSON null is absence; every present non-null pool
field must be a valid normalized id even when a higher-precedence field is also
present. One explicit pool observation replaces that model's former binding;
an explicitly reported set means all listed pools apply.

Every bound pool must be present. Only windows within five percent of 604,800
seconds participate. When several applicable weekly constraints exist, the
worst state wins and far-under is possible only when all are far-under. Missing,
stale, or timing-untrusted members make the result unknown. Once the selected
model's provider has published quota current state, an absent binding or bound
weekly window is likewise neutral unknown; it never becomes a colored claim.
Providers with no quota current-state capability do not gain a quota chip.
Once observed, provider quota capability lasts for the running harness.
Clearing an account snapshot removes its windows and bindings but retains and
replays an empty capability snapshot, so live and late-selected clients both
show neutral unknown rather than disagreeing about applicability.

## Pacing

For used fraction `u`, duration `D`, and trustworthy remaining seconds `q`,
elapsed fraction is `e = clamp(1 - q/D, 0, 1)` and deviation is `d = u-e`.

- danger: `u >= 90%` or `d >= +25` percentage points;
- over: `d >= +10` points;
- far under: `e >= 10%` and `d <= -15` points;
- otherwise aligned.

Worsening transitions occur immediately. Exit hysteresis is three percentage
points: far-under exits at `-12`, over exits at `+7`, and danger remains while
either `d > +22` or `u >= 87%`. A verified new reset cycle clears hysteresis.

Relative server remaining time is preferred. Absolute reset time requires a
fresh server-offset calibration; absolute/relative disagreement beyond five
minutes is untrusted. Usage, timing, and applicability are colored only through
15 minutes. After that the chip is neutral `Q?`, including after hard staleness.
At or past reset, Tau shows neutral unknown and requests reconciliation; it never
fabricates a local zero-use rollover.

## Presentation

The compact accessible chip is bright-green `Q-` for far under, green `Q=` for
aligned, orange/yellow `Q+` for over, red `Q!` for danger, and neutral `Q?` for
stale or unknown state. Text carries the meaning in no-color and limited-color
terminals. The status bar drops this optional chip before essential identity on
narrow displays. No playful or “tokenmaxxing” copy is part of this design.
