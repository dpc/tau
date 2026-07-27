# SPEC-provider-response-streaming: Provider response streaming

Providers submit `provider.response_updated_reported` as transient, prompt-local
progress. After generic report commit and prompt-owner validation, the harness publishes
canonical `provider.response_updated`. Assistant text and reasoning use append-only
deltas; provider-authored status and retry diagnostics stay in the separate status
field. The first non-empty progress sample may be emitted promptly. Later nonterminal
output, status, and stats updates are sampled at most once per second per prompt, with
one immediate terminal flush permitted before closure.

Providers accumulate arbitrary upstream chunks, transport bytes, visible text,
compaction status, and non-visible semantic output independently of that public
cadence. The rate-limited emitter, not the parser's chunk cadence, decides when
to publish an update.

`provider.response_finished.output_items` is the complete durable replacement and replay source. Consumers clear transient state when an attempt restarts or repetition is rejected, and replace rather than append that state when the terminal response arrives. A valid update observed after late subscription may create an ellipsis-prefixed transient block for an otherwise unknown live prompt. Stale, already-finished, or invalid prompt updates do not create transcript state.

Providers own transport-byte accounting and publish content-free previous/current cumulative `response_stats`; `previous` is the last sample actually emitted. The harness validates provider source and prompt ownership, derives the public agent id from harness state, and broadcasts accepted updates without becoming the accounting authority. Provider-authored errors, bodies, headers, prompt text, tool data, account identifiers, and secrets never enter public stats.

Stats count backend response bytes received at the provider transport before
semantic parsing. They exclude prompts, tool execution output and results, UI
rendering text, and Tau UI/harness protocol bytes, and are never folded into
transcripts. The harness validates prompt ownership and cancellation before
deriving public routing identity and broadcasting the samples unchanged.

Report/canonical authority and terminal correlation are specified by
[SPEC-provider-execution-reports-and-canonical-facts](SPEC-provider-execution-reports-and-canonical-facts.md).

First-party providers apply a bounded exact-repetition guard to assistant text, reasoning summaries, and tool-argument deltas before acceptance. Detection clears transient output and terminates with the closed `repetition_detected` failure rather than treating repeated text as durable model output.

Watcher projection is separately governed by [SPEC-agent-watch](SPEC-agent-watch.md); recovery and compaction authority are governed by [SPEC-compaction-and-context-recovery](SPEC-compaction-and-context-recovery.md).
