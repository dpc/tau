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
Last completed: 2026-08-13

Compare Tau's complete ChatGPT/Codex integration with current upstream Codex: review all relevant upstream changes and Tau's corresponding request, response, authentication, capability, and error-handling paths. Test or record material differences, create follow-ups as needed, then replace the date above with the completion date.


## Weekly: selfci runtime review

Frequency: Weekly  
Last completed: 2026-08-26

Run or inspect a representative `selfci check`, review its total and stage runtimes, and look for obvious regressions or optimization opportunities. Record or create follow-ups for worthwhile work, then replace `Never` (or the prior date) with the completion date.

Keep one comparable baseline per review below. Record the lane, candidate,
material cache state, host class, result, wall time, and SelfCI's slowest
concurrent jobs or steps. Replace older rows when the table grows beyond eight
entries; keep detailed logs outside the repository.

| Date | Lane, candidate, and cache state | Host | Result | Wall | Slow jobs/steps |
| --- | --- | --- | --- | --- | --- |
| 2026-08-26 | Routine (`TAU_CI_FULL` unset), `sqvoquny`; workspace, Clippy, and site cached; `ci.tests` cold | Ryzen 9 7950X3D, 32 threads, 61 GiB, Nix 2.35.1 | Expected `tau-session-inspect` checkpoint-oracle failure | 58.5s | Critical cargo 51.6s: test compile 22.5s, tests 18.1s; concurrent/off-path lint 22.4s and site 0.1s |
