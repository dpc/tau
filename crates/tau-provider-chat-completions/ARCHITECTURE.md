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

Streamed assistant text, reasoning text, and function-call argument chunks are
the source for transient `provider.response_updated.progress` metadata while a
turn is in flight. Progress samples are content-free byte counters with
aggregate start/end totals and a sample-window duration; they let UIs show
generic bytes/rates without storing previous samples or exposing response or
argument content in status text.

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
