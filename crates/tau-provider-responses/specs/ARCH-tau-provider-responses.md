# ARCH-tau-provider-responses: Public Responses backend

`tau-provider-responses` owns one finite API-key HTTP/SSE attempt for the
generic public Responses protocol. It is separate from both the generic Chat
Completions backend and the private ChatGPT/Codex WebSocket backend.

The backend replays the complete typed Responses transcript on every request.
It supports text and Function tools only, preserves Responses assistant and
function-call replay sidecars, and never sends `previous_response_id` or
provider-side compaction controls. The extension owns profile storage, model
publication, retry scheduling, cancellation policy, and protocol-event
sampling.
