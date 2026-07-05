# Harness/Agent Item Types

This document is a Rust-shaped companion to
[`harness-agent-item-architecture.md`](harness-agent-item-architecture.md).

It is intentionally illustrative rather than exact compile-ready code. The
goal is to show the core shapes and relationships in a more code-native way.

## Core Transcript Items

```rust
enum ContextItem {
    Message(MessageItem),
    ToolCall(ToolCallItem),
    ToolResult(ToolResultItem),
    Reasoning(OpaqueProviderItem),
    Compaction(OpaqueProviderItem),
    UnknownProviderItem(OpaqueProviderItem),
}

struct MessageItem {
    role: ContextRole,
    content: Vec<ContentPart>,
    phase: Option<MessagePhase>,
    responses_raw_json: Option<String>,
}

enum ContextRole {
    System,
    Developer,
    User,
    Assistant,
}

struct ToolCallItem {
    call_id: ToolCallId,
    name: ToolName,
    tool_type: ToolType,
    arguments: CborValue,
    raw_arguments_json: Option<String>,
}

struct ToolResultItem {
    call_id: ToolCallId,
    tool_type: ToolType,
    status: ToolResultStatus,
    output: CborValue,
}

enum ToolResultStatus {
    Success,
    Error { message: String },
    Cancelled { reason: String },
}

struct OpaqueProviderItem(CborValue);
```

Notes:

- `ContextItem` is the provider-ish item timeline Tau reasons about.
- `MessageItem::responses_raw_json` is an optional Responses assistant-message
  replay sidecar. It preserves provider-owned item envelope/content fields for
  cache and replay fidelity, while typed `role`, `content`, and `phase` remain
  semantic truth.
- `ToolCallItem::name` is already the strict Tau-visible tool name. Provider
  names that cannot be normalized to a valid `ToolName` are rejected before
  becoming transcript items.
- `ToolCallItem::arguments` is the parsed CBOR semantic form used for
  validation, routing, and tool dispatch. `raw_arguments_json` is an optional
  provider-wire sidecar for function calls whose upstream argument payload was a
  JSON string; provider replay uses it to preserve cache identity and falls back
  to serializing `arguments` only for old records or non-function/custom calls.
- `Reasoning`, `Compaction`, and `UnknownProviderItem` are opaque payloads.
- Embedded `ContextItem::Compaction` is preserved as output, but is not a
  transcript boundary.

## Transcript Nodes

```rust
enum TranscriptNode {
    UserInput(UserInputNode),
    AssistantResponse(AssistantResponseNode),
    ToolResults(ToolResultsNode),
    Compaction(CompactionNode),
}

struct UserInputNode {
    items: Vec<ContextItem>,
}

struct AssistantResponseNode {
    provider_response_id: Option<String>,
    backend: Option<ProviderBackendRef>,
    output_items: Vec<ContextItem>,
    usage: Option<TokenUsage>,
}

struct ToolResultsNode {
    items: Vec<ToolResultItem>,
}

struct CompactionNode {
    replacement_window: Vec<ContextItem>,
}
```

Structural invariants:

- `AssistantResponseNode.output_items` preserves exact assistant output order.
- `ToolResultsNode` is the direct child of the `AssistantResponseNode` whose
  tool calls it completes.
- No separate `responds_to` field is needed on `ToolResultsNode`.

## Durable Turn-Shaped Prompt

`AgentPromptCreated` remains an operational prompt-delivery event rather than
durable transcript truth, but its shape is still central to the model.

```rust
struct AgentPromptCreated {
    agent_prompt_id: AgentPromptId,
    agent_id: AgentId,
    system_prompt: String,
    // Fully materialized history for this turn.
    context_items: Vec<ContextItem>,
    tools: Vec<ToolDefinition>,
    tools_ref: Option<PromptToolsRef>,
    model: Option<ModelId>,
    model_params: ModelParams,
    tool_choice: ToolChoice,
    originator: PromptOriginator,
    previous_response_candidate: Option<PreviousResponseCandidate>,
}

struct PreviousResponseCandidate {
    provider_response_id: String,
    next_item_index: usize,
    backend: ProviderBackendRef,
}
```

Rules:

- `context_items` must be sufficient even when the candidate is ignored.
- `context_items` is the full effective history for the turn, not a suffix.
- The candidate is only an optimization hint.
- Runtime validity of `previous_response_id` remains agent-side state rather
  than part of the shared prompt type model.

## Durable Transcript Facts

These are the durable transcript inputs:

```rust
enum PersistedTranscriptFact {
    AgentPromptSubmitted(AgentPromptSubmitted),
    AgentUserMessageInjected(AgentUserMessageInjected),
    AgentPromptSteered(AgentPromptSteered),
    ProviderResponseFinished(ProviderResponseFinished),
    ToolResult(ToolResultFact),
    AgentCompacted(AgentCompacted),
}

struct ProviderResponseFinished {
    agent_prompt_id: AgentPromptId,
    agent_id: AgentId,
    originator: PromptOriginator,
    backend: Option<ProviderBackendRef>,
    provider_response_id: Option<String>,
    output_items: Vec<ContextItem>,
    usage: Option<TokenUsage>,
}

struct ToolResultFact {
    call_id: ToolCallId,
    tool_type: ToolType,
    status: ToolResultStatus,
    output: CborValue,
}

struct AgentCompacted {
    agent_id: AgentId,
    replacement_window: Vec<ContextItem>,
}
```

Operational-only events such as `ToolRequest`, `AgentPromptQueued`, and
progress events stay out of the semantic transcript model. `ToolRequest`
is runtime routing intent, not assistant-output truth.

The bus-level runtime `ToolResult` event may still carry extra operational
fields such as `tool_name`, `result`, `display`, and `originator` for UI
rendering and tool plumbing. `ToolResultFact` above is the narrower durable
completed-tool fact shape the projection reasons about.

## Fold State

Replay and incremental updates need explicit state in addition to the already
emitted nodes.

```rust
struct ProjectionState {
    conversations: HashMap<ConversationKey, ConversationProjectionState>,
}

struct ConversationProjectionState {
    open_tool_round: Option<PendingToolRound>,
}

struct PendingToolRound {
    assistant_node_id: NodeId,
    call_order: Vec<ToolCallId>,
    terminal_results: HashMap<ToolCallId, ToolResultItem>,
}
```

Notes:

- This is fold-time state, not durable wire state.
- It is acceptable to rely on explicit buffering here.
- `call_id` must be globally unique for tool calls.
- If a durable `ToolResultFact` cannot be matched to the currently open call
  set, replay should fail for that agent.

## Runtime-Only Conversation State

The harness still needs live routing and scheduling state that is not part of
the durable transcript model.

```rust
struct ConversationRuntimeState {
    head: Option<NodeId>,
    turn_state: ConversationTurnState,
    pending_prompts: VecDeque<String>,
    chain_anchor: Option<ChainAnchor>,
}

enum ConversationTurnState {
    Idle,
    AgentThinking {
        agent_prompt_id: AgentPromptId,
    },
    ToolsRunning {
        remaining_calls: Vec<ToolCallId>,
    },
    Compacting,
}
```

Rules:

- A single conversation may have at most one unresolved tool round at a time.
- A queued prompt may later become a durable `AgentPromptSteered`.
- Queued prompts remain runtime-only until that moment.
- Queued prompts may be discarded on cancellation or restart.

## Validation Rules

```rust
enum AgentOutputValidation {
    Valid,
    ValidButToolRejected,
    StructurallyMalformed,
}
```

Intent:

- valid assistant tool calls to unavailable or denied tools still commit and
  later receive terminal error results
- recoverable malformed call ids (empty, duplicate within the response, or
  reused from a prior tool call) are repaired to synthetic ids and commit with
  matching terminal error results so the next prompt has paired tool-call/error
  items
- unrecoverable malformed provider output does not commit as a successful
  assistant response

## Compaction Rules

```rust
struct CompactionBoundary {
    replacement_window: Vec<ContextItem>,
}
```

Only standalone compaction creates a boundary.

- `AgentCompacted -> CompactionNode { replacement_window }`
- request assembly stops walking older history at that node
- embedded `ContextItem::Compaction` remains plain opaque output

## Minimal Mental Model

If the whole design is reduced to a few Rust ideas, it is this:

```rust
// assistant output becomes one node immediately
AssistantResponseNode {
    output_items: vec![
        ContextItem::ToolCall(call_a),
        ContextItem::ToolCall(call_b),
    ],
}

// tool results arrive one-by-one as durable facts
ToolResultFact { call_id: "a", status: Success, ... }
ToolResultFact { call_id: "b", status: Error { .. }, ... }

// the fold buffers them and later emits one grouped node
ToolResultsNode {
    items: vec![result_a, result_b],
}
```

That is the core shape the rest of the architecture is built around.
