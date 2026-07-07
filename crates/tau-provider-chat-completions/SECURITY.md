# tau-provider-chat-completions security notes

This crate sends prompt/tool context to OpenAI-compatible Chat Completions
providers and parses provider responses back into Tau stream state. Treat
upstream responses and diagnostics as crossing an external-provider trust
boundary.

## Streaming status boundary

Providers must not copy raw streamed text, reasoning, or function-call argument
chunks into status text, logs, notices, or UI-only diagnostics. Live byte stats
are harness-owned `agent.turn_stats_updated` events, not public provider
metadata. The private `semantic_output.non_visible_output_bytes` snapshot sent to
the harness is content-free, cumulative for the current provider prompt, excluded
from durable/public outputs, and stripped before subscriber delivery. The parsed
`ToolCallItem.arguments` and `raw_arguments_json` fields remain the provider/tool
replay surface once the tool call is complete.
