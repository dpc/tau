# tau-provider-chat-completions security notes

This crate sends prompt/tool context to OpenAI-compatible Chat Completions
providers and parses provider responses back into Tau stream state. Treat
upstream responses and diagnostics as crossing an external-provider trust
boundary.

## Streaming status boundary

Providers must not copy raw streamed text, reasoning, or function-call argument
chunks into status text, logs, notices, or UI-only diagnostics. Provider response
stats are private, content-free provider-to-harness metadata: providers own the
prompt-local byte counter, emit previous/current samples at no more than 1Hz
except the final flush, and the harness validates ownership and strips the field
before public provider delivery. The private
`semantic_output.non_visible_output_bytes` snapshot sent to the harness is also
content-free, cumulative for the current provider prompt, excluded from
durable/public outputs, and stripped before subscriber delivery. The parsed
`ToolCallItem.arguments` and `raw_arguments_json` fields remain the provider/tool
replay surface once the tool call is complete.
