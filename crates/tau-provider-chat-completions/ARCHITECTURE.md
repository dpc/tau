# tau-provider-chat-completions architecture

This crate contains OpenAI-compatible Chat Completions request construction,
stream parsing, provider-model publication helpers, and replay conversion shared
by built-in Chat Completions and OpenRouter-style profiles.

## Function-call argument replay identity

Chat Completions providers expose function-call arguments as JSON text. The
stream parser must keep the exact provider string in
`ToolCallItem.raw_arguments_json` while also parsing it into
`ToolCallItem.arguments` for validation and tool dispatch.

Assistant tool-call replay must prefer `raw_arguments_json` when present so
provider-visible history preserves argument key order, whitespace, and numeric
spelling. Serializing parsed CBOR is only a fallback for old persisted records or
records that never had provider-wire JSON.

Streamed assistant text and reasoning text are emitted as append deltas only.
Providers no longer publish public byte-progress metadata. For streamed
tool-call arguments, Chat Completions sends the harness a private, content-free
`semantic_output.non_visible_output_bytes` snapshot that is cumulative for the
current provider prompt, not a per-update delta. The harness strips that snapshot
before subscriber delivery and surfaces any public liveness display only through
`agent.turn_stats_updated`.

Streaming parsers may receive upstream chunks at arbitrary cadence, but Tau
protocol updates are sampled. The provider response sampler starts when the
backend request is dispatched. Chunk reads only update in-memory cumulative
state and pending visible/non-visible deltas. The rate-limited emitter writes a
non-terminal `provider.response_updated` sample only on one-second response
deadlines; byte changes never bypass that cadence. Each private `response_stats`
pair uses `previous` = the last provider sample actually emitted for the prompt
and `current` = the new cumulative sample. A terminal flush is the only normal
bypass and is allowed immediately before the provider prompt closes.

## Transcript replay boundary

Chat Completions replay reconstructs provider-visible history from Tau's
semantic transcript, not from an opaque provider-wire transcript snapshot. Tau
preserves the Chat Completions `messages[]` meaning it needs to continue the
conversation:

- message roles and visible assistant/user/system text;
- assistant reasoning text when a compatible provider exposes it;
- assistant tool calls and terminal tool results;
- raw function-call argument JSON strings, via `ToolCallItem.raw_arguments_json`,
  because providers observe the argument string spelling during replay.

Tau does not currently preserve arbitrary provider-specific assistant-message
fields that are outside those semantics, such as unknown `message` object
members returned by a particular OpenAI-compatible server. Those fields are not
part of the shared Chat Completions transcript contract in Tau. Add an opaque
sidecar only when a concrete provider proves that a specific field is necessary
for replay, cache identity, or correctness; until then, typed semantic transcript
items remain the source of truth.
