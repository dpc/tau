//! Types shared by the ChatGPT/Codex Responses transports.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tau_proto::{
    CborValue, ContentPart, ContextItem, ContextRole, MessageItem, OpaqueProviderItem,
    PromptContext, PromptOriginator, ProviderResponseCompactionStatus,
    ProviderResponseCompactionUpdate, ProviderResponseSemanticOutput, ProviderResponseTextDelta,
    ProviderTokenUsage, ReasoningTextItem, ReasoningTextKind, ResponsesToolCallEnvelope, SessionId,
    ToolCallItem, ToolDefinition,
};
use tau_provider::{StreamRepetitionGuard, StreamRepetitionKey};
use uuid::Uuid;

/// The parts of a prompt needed by an LLM backend client.
pub struct PromptPayload<'a> {
    pub system_prompt: &'a str,
    pub context: &'a PromptContext,
    pub tools: &'a [ToolDefinition],
    /// Per-prompt model knobs (effort / verbosity / thinking-summary).
    /// Each field is honored only when the backend's config reports
    /// support for the corresponding provider feature.
    pub params: tau_proto::ModelParams,
    /// Whether the model may emit tool-call output on this turn.
    /// `Auto` (default) lets the model decide; `None` forces a text
    /// answer. Tools and `system_prompt` are still passed verbatim
    /// in either case so the prompt-cache prefix stays stable.
    pub tool_choice: tau_proto::ToolChoice,
    /// Server-side context-management compaction metadata, when enabled for
    /// this prompt/model.
    pub compaction: Option<tau_proto::PromptCompactionContext>,

    /// Who originated this prompt — the interactive user, or a side query
    /// such as a harness-owned delegated sub-agent. This remains available for
    /// provenance, retry policy, and provider lifecycle events, but it must not
    /// affect prompt-cache routing.
    pub originator: &'a PromptOriginator,
    /// Legacy request flag for callers that once requested the user's cache
    /// bucket explicitly. Prompt-cache routing is now stable per agent, so this
    /// no longer changes the wire `prompt_cache_key`.
    pub share_user_cache_key: bool,
    /// Harness session this prompt belongs to. Used for debug paths and
    /// tracing; the Responses WebSocket pool keys upstream sockets by the
    /// prompt-cache UUID instead.
    /// Backends without session-scoped diagnostics ignore this.
    pub session_id: &'a SessionId,
    /// Durable agent this prompt belongs to.
    pub agent_id: &'a tau_proto::AgentId,
    /// Whether provider debug request/response captures may be written for this
    /// prompt's current session.
    ///
    /// The harness must supply this from explicit session persistence state.
    /// Backends must not infer durability from filesystem paths because an
    /// ephemeral session can reuse a session id that already has durable state
    /// from an earlier run.
    pub debug_provider_requests: bool,
}

impl PromptPayload<'_> {
    /// Derive the OpenAI-style prompt-cache UUID for this prompt on `base_url`.
    ///
    /// ChatGPT WebSocket upgrades use the same UUID for their upstream
    /// `session-id` and `thread-id` headers, so callers should derive the value
    /// through this method rather than duplicating the hashing inputs.
    #[must_use]
    pub fn prompt_cache_key(&self, base_url: &str) -> String {
        prompt_cache_key_for(base_url, self.agent_id)
    }
}

/// Transport / protocol error returned from any LLM backend stream.
#[derive(Debug)]
pub enum LlmError {
    Http(Box<ureq::Error>),
    HttpStatus(u16, String),
    Io(std::io::Error),
    Json(serde_json::Error),
    Vcr(tau_vcr::VcrError),
    RepetitionDetected(tau_provider::StreamRepetition),
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http(e) => write!(f, "HTTP error: {e}"),
            Self::HttpStatus(code, body) => write!(f, "HTTP {code}: {body}"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Json(e) => write!(f, "JSON error: {e}"),
            Self::Vcr(e) => write!(f, "VCR error: {e}"),
            Self::RepetitionDetected(repetition) => write!(f, "{repetition}"),
        }
    }
}

impl std::error::Error for LlmError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Http(e) => Some(e),
            Self::Io(e) => Some(e),
            Self::Json(e) => Some(e),
            Self::Vcr(e) => Some(e),
            Self::RepetitionDetected(_) => None,
            Self::HttpStatus(_, _) => None,
        }
    }
}

impl LlmError {
    /// Whether this error is plausibly transient and worth retrying.
    ///
    /// We treat transport hiccups, mid-stream IO breaks, and
    /// server-side stream errors (overload, upstream timeout) as
    /// retryable. JSON parse failures, missing-choices, and 4xx
    /// statuses other than 408/425/429 are treated as our bug or a
    /// deterministic request-level rejection — retrying just burns
    /// quota.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Http(_) => Some(Duration::ZERO),
            Self::Io(_) => Some(Duration::ZERO),
            Self::Json(_) | Self::Vcr(_) | Self::RepetitionDetected(_) => None,
            Self::HttpStatus(code, body) => match *code {
                408 | 425 => Some(Duration::ZERO),
                429 => usage_limit_retry_after(body),
                500..=599 => Some(Duration::ZERO),
                // Code 0 is synthesized by the Responses backend for
                // SSE-level events: the body is prefixed with
                // "stream error:" (mid-stream provider hiccup —
                // overload, upstream timeout, gateway reset),
                // "response failed:" (deterministic model error),
                // or "response incomplete:" (request-level cap).
                // Only the first class is worth retrying — and even
                // then, account-level caps (usage_limit_reached,
                // rate_limit_exceeded, quota_exceeded) arrive
                // through this path as "stream error: …" and are
                // *not* transient. Local provider-stream idle watchdog
                // timeouts also use the same prefix but are terminal
                // for the current turn so queued prompts unblock
                // promptly instead of waiting through another full
                // idle window. Upstream error types are tagged in the
                // body suffix by `responses::apply_event`.
                0 if body.starts_with("stream error:") => {
                    if is_account_limit_body(body) || is_provider_stream_idle_timeout_body(body) {
                        None
                    } else {
                        Some(Duration::ZERO)
                    }
                }
                _ => None,
            },
        }
    }
}

/// Account-level limits that won't clear with any reasonable backoff —
/// matched against the `(type=…)` suffix that
/// `responses::apply_event` attaches to a `stream error: …` body. New
/// upstream error types can be added here as we encounter them;
/// false negatives just mean we burn a few retries before failing.
///
/// Exposed for the WS pool's `is_recoverable_ws_error` carve-out so
/// the pool doesn't reopen a socket just to hit the same cap on the
/// fresh one.
pub fn is_account_limit_body(body: &str) -> bool {
    body.contains("(type=usage_limit_reached)")
        || body.contains("(type=rate_limit_exceeded)")
        || body.contains("(type=quota_exceeded)")
        || body.contains("(type=billing_hard_limit_reached)")
        || body.contains("(type=insufficient_quota)")
}

/// Provider stream idle watchdog failures are terminal for the current turn.
///
/// Retrying would keep a visibly stalled prompt in-flight for another full idle
/// window instead of promptly unblocking queued work with a terminal provider
/// error.
pub fn is_provider_stream_idle_timeout_body(body: &str) -> bool {
    body.contains("provider stream idle timeout")
}

fn usage_limit_retry_after(body: &str) -> Option<Duration> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let error = value.get("error")?;
    if error.get("type")?.as_str()? != "usage_limit_reached" {
        return None;
    }
    if let Some(seconds) = error
        .get("resets_in_seconds")
        .and_then(serde_json::Value::as_u64)
    {
        return Some(Duration::from_secs(seconds));
    }
    let resets_at = error.get("resets_at")?.as_u64()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(Duration::from_secs(resets_at.saturating_sub(now)))
}

/// One provider output item as it is incrementally assembled from a
/// streaming response. This is intentionally item-shaped: final
/// `ProviderResponseFinished.output_items` must be a projection of the
/// stream's item timeline, not a late re-bucketing of text/reasoning/tool
/// calls.
#[derive(Clone, Debug)]
pub enum OutputItemAccumulator {
    Empty,
    Message(MessageAccumulator),
    ToolCall(ToolCallAccumulator),
    Reasoning(OpaqueProviderItem),
    Compaction(Option<OpaqueProviderItem>),
    UnknownProviderItem(OpaqueProviderItem),
}

/// Accumulates one assistant message item across text deltas.
#[derive(Clone, Debug, Default)]
pub struct MessageAccumulator {
    /// Accumulated visible assistant text for this output item.
    pub text: String,
    /// Optional Responses assistant message phase captured for this item.
    pub phase: Option<tau_proto::MessagePhase>,
    /// Raw Responses assistant message item used for replay fidelity.
    pub responses_raw_json: Option<String>,
}

/// Accumulated streaming state shared by both backends.
pub struct StreamState {
    /// Concatenated visible assistant text, kept for backend validation and
    /// tests. The durable final output is assembled from `output_items`.
    pub text: String,
    pub output_items: Vec<OutputItemAccumulator>,
    pub input_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// Provider-supplied reasoning summary accumulated so far. `None`
    /// when the provider hasn't emitted any summary content (or when
    /// summaries weren't requested).
    pub thinking: Option<String>,
    /// Output item index the displayable reasoning summary belongs to.
    thinking_output_index: Option<usize>,
    /// Provider-supplied `response.id`, used by the harness to chain
    /// the next turn off this one via `previous_response_id`. Only
    /// populated by the Responses backend; the Chat Completions
    /// backend leaves this `None`.
    pub response_id: Option<String>,
    /// Raw terminal provider event for Responses streams (`response.completed`
    /// / `response.done`), retained for per-session debug captures. Other
    /// backends leave this empty.
    pub provider_terminal_event: Option<serde_json::Value>,
    /// A stale `previous_response_id` was rejected and this successful stream
    /// came from the full-replay retry.
    pub stale_chain_fallback: bool,
    /// Synthesized item slot for plain assistant text content.
    chat_message_item_index: Option<usize>,
    /// Bounded exact repetition guard for this provider generation.
    repetition_guard: StreamRepetitionGuard,
}

/// Tracks text already emitted to transient response update streams.
#[derive(Debug, Default)]
pub struct StreamDeltaEmitter {
    /// Last assistant message text emitted per provider output index.
    emitted_messages: BTreeMap<usize, String>,
    /// Last reasoning text emitted per provider output index.
    emitted_reasoning: BTreeMap<usize, String>,
}

impl StreamDeltaEmitter {
    /// Returns newly appended assistant/reasoning deltas since the last call.
    pub fn deltas(&mut self, state: &StreamState) -> Vec<ProviderResponseTextDelta> {
        let mut deltas = Vec::new();
        for (index, item) in state.output_items.iter().enumerate() {
            if let OutputItemAccumulator::Message(message) = item {
                self.push_message_delta(index, message, &mut deltas);
            }
        }
        if let Some(thinking) = state
            .thinking
            .as_deref()
            .filter(|thinking| !thinking.is_empty())
        {
            let index = state.thinking_output_index.unwrap_or(0);
            let previous = self.emitted_reasoning.entry(index).or_default();
            if let Some(delta) = append_suffix(previous, thinking) {
                deltas.push(ProviderResponseTextDelta::ReasoningText {
                    output_index: index as u32,
                    kind: ReasoningTextKind::Summary,
                    text: delta,
                });
            }
        }
        deltas
    }

    fn push_message_delta(
        &mut self,
        index: usize,
        message: &MessageAccumulator,
        deltas: &mut Vec<ProviderResponseTextDelta>,
    ) {
        if message.text.is_empty() {
            return;
        }
        let previous = self.emitted_messages.entry(index).or_default();
        if let Some(delta) = append_suffix(previous, &message.text) {
            deltas.push(ProviderResponseTextDelta::Message {
                output_index: index as u32,
                text: delta,
                phase: message.phase,
            });
        }
    }
}

fn append_suffix(previous: &mut String, current: &str) -> Option<String> {
    if current == previous {
        return None;
    }
    if let Some(suffix) = current.strip_prefix(previous.as_str()) {
        previous.push_str(suffix);
        if suffix.is_empty() {
            None
        } else {
            Some(suffix.to_owned())
        }
    } else {
        tracing::trace!("stream text stopped being append-only; waiting for final response");
        None
    }
}

/// Accumulates one tool call across streaming chunks.
#[derive(Clone, Debug)]
pub struct ToolCallAccumulator {
    pub id: String,
    pub name: String,
    pub tool_type: tau_proto::ToolType,
    pub arguments_json: String,
    pub responses_envelope: ResponsesToolCallEnvelope,
}

impl ToolCallAccumulator {
    pub fn new(tool_type: tau_proto::ToolType) -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            tool_type,
            arguments_json: String::new(),
            responses_envelope: ResponsesToolCallEnvelope::default(),
        }
    }

    fn context_item(&self) -> Option<ContextItem> {
        if self.name.is_empty() {
            return None;
        }
        let arguments = match self.tool_type {
            tau_proto::ToolType::Function => {
                let args: serde_json::Value =
                    serde_json::from_str(&self.arguments_json).unwrap_or(serde_json::Value::Null);
                json_to_cbor(&args)
            }
            tau_proto::ToolType::Custom => CborValue::Text(self.arguments_json.clone()),
        };
        let name = tau_proto::ToolName::try_new(self.name.clone())?;
        Some(ContextItem::ToolCall(ToolCallItem {
            call_id: self.id.clone().into(),
            name,
            tool_type: self.tool_type,
            arguments,
            raw_arguments_json: (self.tool_type == tau_proto::ToolType::Function)
                .then(|| self.arguments_json.clone()),
            responses_envelope: (!self.responses_envelope.is_empty())
                .then(|| self.responses_envelope.clone()),
        }))
    }
}

impl OutputItemAccumulator {
    fn context_item(&self) -> Option<ContextItem> {
        match self {
            OutputItemAccumulator::Empty => None,
            OutputItemAccumulator::Message(message) => (!message.text.is_empty()).then(|| {
                assistant_text_item_with_phase_and_raw(
                    message.text.clone(),
                    message.phase,
                    message.responses_raw_json.clone(),
                )
            }),
            OutputItemAccumulator::ToolCall(call) => call.context_item(),
            OutputItemAccumulator::Reasoning(item) => Some(ContextItem::Reasoning(item.clone())),
            OutputItemAccumulator::Compaction(Some(item)) => {
                Some(ContextItem::Compaction(item.clone()))
            }
            OutputItemAccumulator::Compaction(None) => None,
            OutputItemAccumulator::UnknownProviderItem(item) => {
                Some(ContextItem::UnknownProviderItem(item.clone()))
            }
        }
    }
}

impl Default for StreamState {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamState {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            output_items: Vec::new(),
            input_tokens: None,
            cached_tokens: None,
            output_tokens: None,
            thinking: None,
            thinking_output_index: None,
            response_id: None,
            provider_terminal_event: None,
            stale_chain_fallback: false,
            chat_message_item_index: None,
            repetition_guard: StreamRepetitionGuard::new(),
        }
    }

    pub fn check_message_delta(
        &mut self,
        output_index: usize,
        delta: &str,
    ) -> Result<(), LlmError> {
        if let Some(repetition) = self
            .repetition_guard
            .push_delta(StreamRepetitionKey::AssistantText { output_index }, delta)
        {
            return Err(LlmError::RepetitionDetected(repetition));
        }
        Ok(())
    }

    pub fn check_message_snapshot(
        &mut self,
        output_index: usize,
        text: &str,
    ) -> Result<(), LlmError> {
        let current = self.message_text_at(output_index).unwrap_or_default();
        if let Some(delta) = text.strip_prefix(current) {
            self.check_message_delta(output_index, delta)
        } else if let Some(repetition) = self
            .repetition_guard
            .replace_tail(StreamRepetitionKey::AssistantText { output_index }, text)
        {
            Err(LlmError::RepetitionDetected(repetition))
        } else {
            Ok(())
        }
    }

    pub fn check_reasoning_delta(
        &mut self,
        output_index: usize,
        delta: &str,
    ) -> Result<(), LlmError> {
        if let Some(repetition) = self
            .repetition_guard
            .push_delta(StreamRepetitionKey::ReasoningText { output_index }, delta)
        {
            return Err(LlmError::RepetitionDetected(repetition));
        }
        Ok(())
    }

    pub fn check_function_arguments_delta(
        &mut self,
        output_index: usize,
        delta: &str,
    ) -> Result<(), LlmError> {
        if let Some(repetition) = self.repetition_guard.push_delta(
            StreamRepetitionKey::FunctionCallArguments { output_index },
            delta,
        ) {
            return Err(LlmError::RepetitionDetected(repetition));
        }
        Ok(())
    }

    pub fn check_function_arguments_snapshot(
        &mut self,
        output_index: usize,
        text: &str,
    ) -> Result<(), LlmError> {
        let current = self.tool_arguments_at(output_index).unwrap_or_default();
        if let Some(delta) = text.strip_prefix(current) {
            self.check_function_arguments_delta(output_index, delta)
        } else if let Some(repetition) = self.repetition_guard.replace_tail(
            StreamRepetitionKey::FunctionCallArguments { output_index },
            text,
        ) {
            Err(LlmError::RepetitionDetected(repetition))
        } else {
            Ok(())
        }
    }

    pub fn check_custom_tool_input_delta(
        &mut self,
        output_index: usize,
        delta: &str,
    ) -> Result<(), LlmError> {
        if let Some(repetition) = self
            .repetition_guard
            .push_delta(StreamRepetitionKey::CustomToolInput { output_index }, delta)
        {
            return Err(LlmError::RepetitionDetected(repetition));
        }
        Ok(())
    }

    pub fn check_custom_tool_input_snapshot(
        &mut self,
        output_index: usize,
        text: &str,
    ) -> Result<(), LlmError> {
        let current = self.tool_arguments_at(output_index).unwrap_or_default();
        if let Some(delta) = text.strip_prefix(current) {
            self.check_custom_tool_input_delta(output_index, delta)
        } else if let Some(repetition) = self
            .repetition_guard
            .replace_tail(StreamRepetitionKey::CustomToolInput { output_index }, text)
        {
            Err(LlmError::RepetitionDetected(repetition))
        } else {
            Ok(())
        }
    }

    fn ensure_output_len(&mut self, output_index: usize) {
        while self.output_items.len() <= output_index {
            self.output_items.push(OutputItemAccumulator::Empty);
        }
    }

    /// Reserves an output-item slot without committing a durable item yet.
    pub fn reserve_output_item_at(&mut self, output_index: usize) {
        self.ensure_output_len(output_index);
    }

    pub fn message_at_mut(&mut self, output_index: usize) -> &mut MessageAccumulator {
        self.ensure_output_len(output_index);
        if !matches!(
            self.output_items[output_index],
            OutputItemAccumulator::Message(_)
        ) {
            self.output_items[output_index] =
                OutputItemAccumulator::Message(MessageAccumulator::default());
        }
        let OutputItemAccumulator::Message(message) = &mut self.output_items[output_index] else {
            unreachable!("message slot was just initialized");
        };
        message
    }

    fn message_text_at(&self, output_index: usize) -> Option<&str> {
        match self.output_items.get(output_index) {
            Some(OutputItemAccumulator::Message(message)) => Some(&message.text),
            _ => None,
        }
    }

    fn tool_arguments_at(&self, output_index: usize) -> Option<&str> {
        match self.output_items.get(output_index) {
            Some(OutputItemAccumulator::ToolCall(call)) => Some(&call.arguments_json),
            _ => None,
        }
    }

    pub fn append_message_delta_at(&mut self, output_index: usize, delta: &str) {
        self.message_at_mut(output_index).text.push_str(delta);
        self.refresh_text();
    }

    pub fn set_message_text_at(&mut self, output_index: usize, text: &str) {
        self.message_at_mut(output_index).text = text.to_owned();
        self.refresh_text();
    }

    pub fn set_message_phase_at(
        &mut self,
        output_index: usize,
        phase: Option<tau_proto::MessagePhase>,
    ) {
        if let Some(phase) = phase {
            self.message_at_mut(output_index).phase = Some(phase);
        }
    }

    /// Stores the raw Responses assistant message item for one output index.
    pub fn set_message_responses_raw_json_at(&mut self, output_index: usize, raw_json: &str) {
        self.message_at_mut(output_index).responses_raw_json = Some(raw_json.to_owned());
    }

    pub fn append_chat_message_delta(&mut self, delta: &str) {
        let output_index = match self.chat_message_item_index {
            Some(output_index) => output_index,
            None => {
                let output_index = self.output_items.len();
                self.output_items
                    .push(OutputItemAccumulator::Message(MessageAccumulator::default()));
                self.chat_message_item_index = Some(output_index);
                output_index
            }
        };
        self.append_message_delta_at(output_index, delta);
    }

    pub fn tool_call_at_mut(
        &mut self,
        output_index: usize,
        tool_type: tau_proto::ToolType,
    ) -> &mut ToolCallAccumulator {
        self.ensure_output_len(output_index);
        if !matches!(
            self.output_items[output_index],
            OutputItemAccumulator::ToolCall(_)
        ) {
            self.output_items[output_index] =
                OutputItemAccumulator::ToolCall(ToolCallAccumulator::new(tool_type));
        }
        let OutputItemAccumulator::ToolCall(call) = &mut self.output_items[output_index] else {
            unreachable!("tool-call slot was just initialized");
        };
        call.tool_type = tool_type;
        call
    }

    pub fn set_reasoning_item_json_at(&mut self, output_index: usize, item: &str) {
        if let Some(item) = opaque_item_from_json(item) {
            self.ensure_output_len(output_index);
            self.output_items[output_index] = OutputItemAccumulator::Reasoning(item);
        }
    }

    pub fn start_compaction_item_at(&mut self, output_index: usize) {
        self.ensure_output_len(output_index);
        if !matches!(
            self.output_items[output_index],
            OutputItemAccumulator::Compaction(_)
        ) {
            self.output_items[output_index] = OutputItemAccumulator::Compaction(None);
        }
    }

    pub fn set_compaction_item_json_at(&mut self, output_index: usize, item: &str) {
        if let Some(item) = opaque_item_from_json(item) {
            self.ensure_output_len(output_index);
            self.output_items[output_index] = OutputItemAccumulator::Compaction(Some(item));
        }
    }

    /// Stores an unrecognized provider output item at its provider index.
    pub fn set_unknown_provider_item_json_at(&mut self, output_index: usize, item: &str) {
        if let Some(item) = opaque_item_from_json(item) {
            self.ensure_output_len(output_index);
            self.output_items[output_index] = OutputItemAccumulator::UnknownProviderItem(item);
        }
    }

    /// Appends displayable reasoning-summary text at the provider output index
    /// it belongs to.
    pub fn append_reasoning_summary_delta_at(&mut self, output_index: usize, delta: &str) {
        self.thinking_output_index.get_or_insert(output_index);
        self.thinking
            .get_or_insert_with(String::new)
            .push_str(delta);
    }

    /// Starts a new reasoning-summary paragraph at the provider output index
    /// it belongs to.
    pub fn start_reasoning_summary_part_at(&mut self, output_index: usize) {
        self.thinking_output_index.get_or_insert(output_index);
        if let Some(thinking) = self.thinking.as_mut()
            && !thinking.is_empty()
            && !thinking.ends_with("\n\n")
        {
            thinking.push_str("\n\n");
        }
    }

    /// Returns a content-free snapshot of non-visible semantic output generated
    /// for this provider prompt.
    pub fn semantic_output_for_update(&self) -> Option<ProviderResponseSemanticOutput> {
        let non_visible_output_bytes: u64 = self
            .output_items
            .iter()
            .filter_map(|item| match item {
                OutputItemAccumulator::ToolCall(call) => Some(call.arguments_json.len() as u64),
                _ => None,
            })
            .sum();
        (non_visible_output_bytes != 0).then_some(ProviderResponseSemanticOutput {
            non_visible_output_bytes,
        })
    }

    /// Returns the cumulative provider-generated semantic-output bytes for this
    /// response, including both visible assistant/reasoning text and
    /// non-visible tool/custom-tool input.
    pub fn response_output_bytes(&self) -> u64 {
        let visible_bytes = self.text.len().saturating_add(
            self.thinking
                .as_ref()
                .map(|thinking| thinking.len())
                .unwrap_or(0),
        );
        let visible_bytes = visible_bytes.try_into().unwrap_or(u64::MAX);
        let non_visible_bytes = self
            .semantic_output_for_update()
            .map(|semantic_output| semantic_output.non_visible_output_bytes)
            .unwrap_or(0);
        visible_bytes.saturating_add(non_visible_bytes)
    }

    fn refresh_text(&mut self) {
        self.text.clear();
        for item in &self.output_items {
            if let OutputItemAccumulator::Message(message) = item {
                self.text.push_str(&message.text);
            }
        }
    }

    /// Returns the current compact compaction status, when a compaction item is
    /// present in the live provider output.
    pub fn compaction_update(&self) -> Option<ProviderResponseCompactionUpdate> {
        self.output_items.iter().find_map(|item| match item {
            OutputItemAccumulator::Compaction(Some(_)) => Some(ProviderResponseCompactionUpdate {
                status: ProviderResponseCompactionStatus::Completed,
                original_input_tokens: None,
                compacted_input_tokens: None,
            }),
            OutputItemAccumulator::Compaction(None) => Some(ProviderResponseCompactionUpdate {
                status: ProviderResponseCompactionStatus::Started,
                original_input_tokens: None,
                compacted_input_tokens: None,
            }),
            _ => None,
        })
    }

    /// Returns the final assistant output items in provider item order.
    ///
    /// Tool-call accumulators with an empty `name` are dropped as stream
    /// artifacts. The streaming paths eagerly create slots from
    /// argument-delta events so the index stays addressable; if the
    /// matching name-carrying event never arrives, shipping it
    /// downstream would surface as an `invalid_tool` rejection in the
    /// harness even though the model never committed a valid call.
    pub fn into_output_items(self) -> Vec<ContextItem> {
        let mut items = Vec::new();
        let thinking_index = self.thinking_output_index.unwrap_or(0);
        let thinking = self.thinking.filter(|thinking| !thinking.is_empty());
        let output_items = self
            .output_items
            .into_iter()
            .map(|item| item.context_item())
            .collect::<Vec<_>>();
        let thinking_len = thinking.as_ref().map(|_| thinking_index + 1).unwrap_or(0);
        let len = output_items.len().max(thinking_len);

        for index in 0..len {
            if index == thinking_index
                && let Some(thinking) = &thinking
            {
                items.push(ContextItem::ReasoningText(ReasoningTextItem {
                    kind: ReasoningTextKind::Summary,
                    text: thinking.clone(),
                }));
            }
            if let Some(item) = output_items.get(index).and_then(Option::as_ref) {
                items.push(item.clone());
            }
        }

        if items.is_empty() && !self.text.is_empty() {
            items.push(assistant_text_item(self.text));
        }

        items
    }

    pub fn usage(&self) -> Option<ProviderTokenUsage> {
        let input = self.input_tokens.unwrap_or(0);
        let cached = self.cached_tokens.unwrap_or(0);
        let output = self.output_tokens.unwrap_or(0);
        if input == 0 && cached == 0 && output == 0 {
            None
        } else {
            Some(ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input,
                prompt_cached_tokens: cached,
                response_received_tokens: output,
                stats: Default::default(),
            })
        }
    }
}

pub fn assistant_text_item(text: impl Into<String>) -> ContextItem {
    assistant_text_item_with_phase(text.into(), None)
}

pub fn assistant_text_item_with_phase(
    text: impl Into<String>,
    phase: Option<tau_proto::MessagePhase>,
) -> ContextItem {
    assistant_text_item_with_phase_and_raw(text, phase, None)
}

/// Builds an assistant text item with optional Responses replay sidecar.
pub fn assistant_text_item_with_phase_and_raw(
    text: impl Into<String>,
    phase: Option<tau_proto::MessagePhase>,
    responses_raw_json: Option<String>,
) -> ContextItem {
    ContextItem::Message(MessageItem {
        role: ContextRole::Assistant,
        content: vec![ContentPart::Text { text: text.into() }],
        phase,
        responses_raw_json,
    })
}

fn opaque_item_from_json(item: &str) -> Option<OpaqueProviderItem> {
    let value: serde_json::Value = serde_json::from_str(item).ok()?;
    Some(OpaqueProviderItem::with_raw_json(
        json_to_cbor(&value),
        item.to_owned(),
    ))
}

/// Maps `Effort` to the wire string the OpenAI Responses /
/// Chat Completions APIs accept. `Off` maps to OpenAI's explicit
/// `none` so provider defaults (for example GPT-5.5's `medium`) do
/// not silently apply.
pub fn effort_wire(level: tau_proto::Effort) -> Option<&'static str> {
    use tau_proto::Effort::*;
    match level {
        Off => Some("none"),
        Minimal => Some("minimal"),
        Low => Some("low"),
        Medium => Some("medium"),
        High => Some("high"),
        XHigh => Some("xhigh"),
    }
}

/// Maps `Verbosity` to the wire string OpenAI's `verbosity` /
/// `text.verbosity` field accepts. There is no "off" sentinel — the
/// caller gates the field on a provider-level `supports_verbosity`
/// flag instead.
pub fn verbosity_wire(level: tau_proto::Verbosity) -> &'static str {
    level.as_openai_wire()
}

/// Derive the wire `prompt_cache_key` for the OpenAI-style provider cache.
///
/// The resulting UUID is version 8 shaped from a deterministic hash of the
/// provider endpoint and durable agent lifetime. Prompt provenance/originator
/// is intentionally excluded so agent-to-agent messages, manager relays, and
/// direct user prompts keep the target agent on the same provider cache bucket.
pub fn prompt_cache_key_for(base_url: &str, agent_id: &tau_proto::AgentId) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(base_url.as_bytes());
    hasher.update(b"agent:");
    hasher.update(agent_id.as_str().as_bytes());

    let mut bytes = [0; 16];
    bytes.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    Uuid::new_v8(bytes).to_string()
}

// ---------------------------------------------------------------------------
// CBOR ↔ JSON value conversion
// ---------------------------------------------------------------------------

pub fn cbor_to_json(v: &CborValue) -> serde_json::Value {
    match v {
        CborValue::Null => serde_json::Value::Null,
        CborValue::Bool(b) => serde_json::Value::Bool(*b),
        CborValue::Integer(i) => {
            let n: i128 = (*i).into();
            serde_json::json!(n)
        }
        CborValue::Float(f) => serde_json::json!(f),
        CborValue::Text(s) => serde_json::Value::String(s.clone()),
        CborValue::Bytes(bytes) => serde_json::Value::String(base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            bytes,
        )),
        CborValue::Array(arr) => serde_json::Value::Array(arr.iter().map(cbor_to_json).collect()),
        CborValue::Map(entries) => {
            let mut map = serde_json::Map::new();
            for (k, v) in entries {
                let key = match k {
                    CborValue::Text(s) => s.clone(),
                    other => format!("{other:?}"),
                };
                map.insert(key, cbor_to_json(v));
            }
            serde_json::Value::Object(map)
        }
        CborValue::Tag(_, inner) => cbor_to_json(inner),
        other => {
            tracing::warn!(target: crate::LOG_TARGET, "unsupported CBOR value in tool input: {other:?}");
            serde_json::Value::Null
        }
    }
}

pub fn json_to_cbor(v: &serde_json::Value) -> CborValue {
    match v {
        serde_json::Value::Null => CborValue::Null,
        serde_json::Value::Bool(b) => CborValue::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                CborValue::Integer(i.into())
            } else if let Some(u) = n.as_u64() {
                CborValue::Integer(u.into())
            } else if let Some(f) = n.as_f64() {
                CborValue::Float(f)
            } else {
                CborValue::Null
            }
        }
        serde_json::Value::String(s) => CborValue::Text(s.clone()),
        serde_json::Value::Array(arr) => CborValue::Array(arr.iter().map(json_to_cbor).collect()),
        serde_json::Value::Object(map) => CborValue::Map(
            map.iter()
                .map(|(k, v)| (CborValue::Text(k.clone()), json_to_cbor(v)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests;
