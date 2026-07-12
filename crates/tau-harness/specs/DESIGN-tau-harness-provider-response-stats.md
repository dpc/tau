# DESIGN-tau-harness-provider-response-stats: Provider response stats are provider-owned public events

Status: confirmed, 2026-07-08, user

Providers own prompt-local response byte counting and rate limiting because they dispatch backend requests and read upstream response bytes at the transport boundary. They may attach `response_stats` previous/current samples to `provider.response_updated`, including stats-only updates with no text deltas. The first non-empty sample may be emitted promptly; later non-terminal samples are emitted at most once per second per prompt, with an optional terminal flush before the provider prompt closes.

The harness must not account, sample, remap, strip, or project provider response throughput. Its role for `provider.response_updated.response_stats` is only the normal provider-event boundary: validate provider prompt ownership/cancellation, rewrite routing identity from prompt ownership, enrich unrelated compaction metadata when applicable, and broadcast the provider-owned sample unchanged to subscribers. UI clients render live response throughput directly from provider events.
