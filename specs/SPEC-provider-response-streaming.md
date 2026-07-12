# SPEC-provider-response-streaming: Provider response streaming

`provider.response_updated` carries transient, prompt-local progress. Assistant text and reasoning use append-only deltas; provider-authored status and retry diagnostics stay in the separate status field. The first non-empty progress sample may be emitted promptly. Later nonterminal output, status, and stats updates are sampled at most once per second per prompt, with one immediate terminal flush permitted before closure.

`provider.response_finished.output_items` is the complete durable replacement and replay source. Consumers clear transient state when an attempt restarts or repetition is rejected, and replace rather than append that state when the terminal response arrives. A valid update observed after late subscription may create an ellipsis-prefixed transient block for an otherwise unknown live prompt. Stale, already-finished, or invalid prompt updates do not create transcript state.

Providers own transport-byte accounting and publish content-free previous/current cumulative `response_stats`; `previous` is the last sample actually emitted. The harness validates provider source and prompt ownership, derives the public agent id from harness state, and broadcasts accepted updates without becoming the accounting authority. Provider-authored errors, bodies, headers, prompt text, tool data, account identifiers, and secrets never enter public stats.

First-party providers apply a bounded exact-repetition guard to assistant text, reasoning summaries, and tool-argument deltas before acceptance. Detection clears transient output and terminates with the closed `repetition_detected` failure rather than treating repeated text as durable model output.

Watcher projection is separately governed by [SPEC-agent-watch](SPEC-agent-watch.md); recovery and compaction authority are governed by [SPEC-compaction-and-context-recovery](SPEC-compaction-and-context-recovery.md).
