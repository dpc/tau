# tau-provider-chat-completions security notes

This crate sends prompt/tool context to OpenAI-compatible Chat Completions
providers and parses provider responses back into Tau stream state. Treat
upstream responses and diagnostics as crossing an external-provider trust
boundary.

## Streaming progress metadata

Provider response progress for streamed assistant text, reasoning text, and
function-call arguments must remain content-free. Emit byte counters,
sample-window durations, output indices, omitted-item counts, and bounded labels
only; never copy raw text/reasoning/argument chunks into progress metadata,
status text, logs, notices, or final transcript rendering. The parsed `ToolCallItem.arguments` and `raw_arguments_json` fields
remain the provider/tool replay surface once the tool call is complete.
