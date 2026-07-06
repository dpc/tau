# tau-provider-chat-completions security notes

This crate sends prompt/tool context to OpenAI-compatible Chat Completions
providers and parses provider responses back into Tau stream state. Treat
upstream responses and diagnostics as crossing an external-provider trust
boundary.

## Streaming status boundary

Providers must not copy raw streamed text, reasoning, or function-call argument
chunks into status text, logs, notices, or UI-only diagnostics. Live byte stats
are harness-owned `agent.turn_stats_updated` events, not provider metadata. The
parsed `ToolCallItem.arguments` and `raw_arguments_json` fields
remain the provider/tool replay surface once the tool call is complete.
