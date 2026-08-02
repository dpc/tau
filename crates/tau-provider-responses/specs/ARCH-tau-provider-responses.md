# ARCH-tau-provider-responses: Public Responses backend

`tau-provider-responses` owns one finite API-key HTTP/SSE attempt for the
generic public Responses protocol. It is separate from both the generic Chat
Completions backend and the private ChatGPT/Codex WebSocket backend.

The backend replays the complete typed Responses transcript on every request.
It supports assistant text, plain `reasoning_text` reasoning, and Function
tools. Plain reasoning produces full displayable reasoning under the existing
thinking-visibility policy and a separate opaque durable item; replay skips the
display companion and prefers the opaque item's Responses sidecar. Encrypted,
summary-only, malformed, or mixed reasoning remains unsupported. The backend
also preserves Responses assistant and function-call replay sidecars and never
sends `previous_response_id` or provider-side compaction controls. The
extension owns profile storage, model publication, retry scheduling,
cancellation policy, and protocol-event sampling.

Every request also lowers the harness-selected effective reasoning effort as
`reasoning.effort`. The public API spells Tau's `off` as `none`; the remaining
canonical levels (`minimal`, `low`, `medium`, `high`, `xhigh`, and `max`) pass
through directly.
