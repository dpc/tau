# DESIGN-tau-provider-chatgpt-stream-update-sampling: Streaming protocol updates are sampled

Status: confirmed, 2026-07-07, user

Streaming parsers may receive upstream chunks at arbitrary cadence, but Tau
protocol updates are sampled. Stream state accumulates lower-layer transport
bytes, visible text, compaction status, and non-visible semantic-output bytes;
the rate-limited emitter decides when `provider.response_updated` is written. The
first non-empty streamed output sample may be emitted promptly so live UIs learn
that output has started. Later non-terminal progress must be emitted at most once
per second per prompt, with a terminal flush also allowed immediately before the
provider prompt closes.
