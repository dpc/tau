# DECISION-tau-ext-rhai-runtime-model: Single-threaded reactive Rhai runtime

Authority: unconfirmed

Rhai policy executes on one runtime thread, with protocol I/O and supervised shell
work on helper threads. Completion is event-driven, sources are drained fairly, and
shutdown synchronously performs bounded cancellation, process-group termination,
and reaping instead of polling.

This keeps scripts non-concurrent while allowing shell and harness activity to make
progress. Runtime and process ownership are documented in
[ARCH-tau-ext-rhai](ARCH-tau-ext-rhai.md).
