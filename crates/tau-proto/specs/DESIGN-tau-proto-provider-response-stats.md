# DESIGN-tau-proto-provider-response-stats: Provider response stats are provider-owned public protocol

Status: confirmed, 2026-07-08, user

Provider response throughput is sampled by the provider because the provider owns backend request dispatch and reads the upstream response stream. Public `provider.response_updated.response_stats` carries content-free previous/current prompt-local samples; `previous` is the last provider sample that was actually emitted and `current` is the new cumulative sample. UI clients render these samples directly from `provider.response_updated`.

Live response stats count backend response bytes received by the provider transport before semantic parsing. They must not count prompts, tool execution output/results, UI rendering text, or Tau UI/harness protocol bytes, and they must not be folded into transcripts.

Provider response/progress updates may publish the first non-empty streamed output sample promptly. Later updates must be rate-limited to at most once per second per prompt, except for a terminal flush immediately before the provider prompt closes. Later byte changes must not bypass this cadence. The harness validates prompt ownership and broadcasts provider stats unchanged; it must not account, strip, remap, or project them.
