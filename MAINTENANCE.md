# Maintenance

## Weekly: opt-in live summary-quality trials

Frequency: Weekly  
Last completed: Never

Optionally compare current summary quality with the versioned synthetic-public
corpus in `crates/tau-summary-eval/`. Live trials are non-authoritative and
non-blocking: deterministic protocol, prompt, replay, corpus-validation, and
offline-scoring tests remain the CI gates. Do not run an external model or judge
unless an operator has reviewed the exact disclosed corpus and unmistakably
accepted provider cost and data-retention terms. Tau provides no automatic live
runner or network fallback.

Record a live candidate set and privacy-minimized result as described in
`crates/tau-summary-eval/README.md`, including the exact provider, model,
model version, generation/judge configuration, UTC date, corpus version, and
required opt-in token. Keep generated text and results outside the repository
unless separately reviewed as public-safe. Compare against prior private
results, record material findings or follow-ups, and update the completion date
only when an operator actually runs the optional trial. Skipping it incurs no
maintenance or CI failure.

## Monthly: ChatGPT/Codex compatibility audit

Frequency: Monthly  
Last completed: 2026-11-08

Compare Tau's complete ChatGPT/Codex integration with current upstream Codex: review all relevant upstream changes and Tau's corresponding request, response, authentication, capability, and error-handling paths. Test or record material differences, create follow-ups as needed, then replace the date above with the completion date.

November 8 source-level audit: compared upstream stable `rust-v0.167.0`
(November 6, `086ccc833bc9cf01024ad2dd6af7dbd421b6bf9a`) and main
(`9ba7a3df527e7bfdf0b7df6ef281212bcd2d5125`, November 8) against the prior
`rust-v0.157.1` baseline. Reviewed request lowering, streaming/replay,
cancellation/repair, compaction, OAuth/account handling, model metadata, quota,
and retry/error paths, alongside the
[ChatGPT/Codex changelog](https://learn.chatgpt.com/docs/changelog) and
[API changelog](https://developers.openai.com/api/docs/changelog).
The audit covered the integration surface, not every upstream application change.

Implemented exact-code corrections confirmed in the private-route client:
`slow_down` now uses existing Throttle policy (including HTTP 503);
`misalignment_policy_violation` uses terminal request rejection; and
`refresh_token_expired` / `refresh_token_invalidated` suppress refresh only for
unchanged credential generations at HTTP 400/401. Recognized credit-balance,
organization-spend, and project-spend exhaustion codes use existing Account
cooldowns, without importing upstream's terminal retry policy. Structured retry
advice, failed-response accounting, and provider-prose privacy remain intact.
Removed retired GPT-5.5, GPT-5.4, and GPT-5.4-mini from ChatGPT model
advertisements only, following the [official model retirement notices](https://learn.chatgpt.com/docs/models);
explicit historical configuration and replay, and separate API support, remain.
September's image concern was already resolved: the exact-model image
input/tool-result allowlist agrees with current upstream source.

Standard remains the default; Lite stays explicit and scoped to the audited
GPT-5.6 family. Upstream catalog hints do not override that gate. Optional
Ultra/custom effort, Ultrafast, code-mode inventories, steering, runtime
attestation, and new image-generation models remain unsupported; they are not
implied compatibility requirements. Main-only `budget_reached` and runtime-auth
denial codes need route-applicability evidence before adoption. Static source
does not certify account entitlement, every model's Standard-route acceptance,
private server limits, or image fidelity. No live/paid provider probe or optional
summary trial was run.

## Weekly: selfci runtime review

Frequency: Weekly  
Last completed: 2026-11-08

Run or inspect a representative `selfci check`, review its total and stage runtimes, and look for obvious regressions or optimization opportunities. Record or create follow-ups for worthwhile work, then replace `Never` (or the prior date) with the completion date.

Keep one comparable baseline per review below. Record the lane, candidate,
material cache state, host class, result, wall time, and SelfCI's slowest
concurrent jobs or steps. Replace older rows when the table grows beyond eight
entries; keep detailed logs outside the repository.

| Date | Lane, candidate, and cache state | Host | Result | Wall | Slow jobs/steps |
| --- | --- | --- | --- | --- | --- |
| 2026-08-26 | Routine (`TAU_CI_FULL` unset), `sqvoquny`; workspace, Clippy, and site cached; `ci.tests` cold | Ryzen 9 7950X3D, 32 threads, 61 GiB, Nix 2.35.1 | Expected `tau-session-inspect` checkpoint-oracle failure | 58.5s | Critical cargo 51.6s: test compile 22.5s, tests 18.1s; concurrent/off-path lint 22.4s and site 0.1s |
| 2026-09-26 | `selfci check --candidate rxtqrxqn` (checked `ea6734f9`, description-only change to `b9d6a33f`); full flag at invocation and derivation cache state not captured | Run host not captured; current local host: Ryzen 9 7950X3D, 32 threads, 61 GiB, Nix 2.35.2 | Passed (operator-reported) | SelfCI 169.7s (shell call ~177s) | Critical cargo 169.7s, Nix checks 169.2s; concurrent lint 38.6s (treefmt 16.6s, ast-grep scan 5.8s), site 0.8s |
| 2026-11-08 | Default `selfci check --candidate qsknyltl`, `TAU_CI_FULL` unset; changed Rust artifacts rebuilt, dependencies cached; measured candidate precedes final comments-only `861b8446` (AST unchanged) | Ryzen 9 7950X3D, 32 threads, 61 GiB | Passed: 8271 workspace tests, 29 configured skips; normal postchecks passed | SelfCI 182.323s / 189.22s wall | Metadata 22.507s; lint 33.901s; Nix cargo checks 135.880s; duplicate inventory 23.474s; workspace tests 29.372s at 32 threads |

November review removed a redundant metadata-fixture compilation and repaired
two process-lifetime races without changing timeouts or resource defaults.
Controlled metadata profiling improved the warm script from 36.7–38.3s to
21.7–22.5s. The default full check above validates the repairs; historical
full-run timings are not controlled performance comparisons. A deliberately warm
unchanged check reused Nix results and took 34.000s internally / 41.10s
wall; it did not rerun the Rust suite. Lint dominated that warm path (33.894s,
including treefmt 10.516s). Metadata remains uncached outside Nix. Moving it
into a correctly keyed derivation is a possible follow-up, not a cache shortcut.
Keep caller timeouts generous rather than weakening gates, and preserve useful
parallelism rather than applying blanket serialization or resource caps.
