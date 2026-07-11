# tau-provider-chat-completions security notes

This crate sends prompt/tool context to OpenAI-compatible Chat Completions
providers and parses provider responses back into Tau stream state. Treat
upstream responses and diagnostics as crossing an external-provider trust
boundary.

## Streaming status boundary

Providers must not copy raw streamed text, reasoning, or function-call argument
chunks into status text, logs, notices, or UI-only diagnostics. Provider response
stats are public, content-free metadata on transient `provider.response_updated`
events: providers own the prompt-local byte counter, may emit the first non-empty
previous/current sample promptly, emit later non-terminal samples at no more than
1Hz, and may emit a final flush. The harness validates ownership and broadcasts
the stats unchanged. Stats must contain only byte counts, elapsed timing, and
routing metadata, never raw provider text, tool arguments, prompt text, or wire
payloads. Parsed `ToolCallItem.arguments` and `raw_arguments_json` remain the
provider/tool replay surface once the tool call is complete.

## Provider error authority

Terminal context classification trusts only canonical OpenAI-style `error.code` and `error.type`;
echoed nested fields and provider prose are not authoritative. Known canonical transient
identifiers override deterministic HTTP status classification and remain retryable.

Watcher-visible retry state never includes the upstream body or human error text;
only the provider's closed structured classification crosses into the harness.
