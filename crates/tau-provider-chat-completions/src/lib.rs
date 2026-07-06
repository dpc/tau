//! OpenAI-compatible Chat Completions backend helpers.

pub mod openrouter;

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tau_proto::{
    AgentPromptId, ContentPart, ContextItem, ContextRole, Event, HarnessInputMessage, ModelId,
    ModelName, ModelTag, OpaqueProviderItem, PeerOutputWriter, ProviderBackend,
    ProviderBackendKind, ProviderBackendTransport, ProviderModelInfo, ProviderName,
    ProviderResponseFinished, ProviderResponseProgressItem, ProviderResponseProgressKind,
    ProviderResponseProgressUpdate, ProviderResponseStatusUpdate, ProviderResponseTextDelta,
    ProviderResponseUpdated, ProviderStopReason, ProviderTokenUsage, ReasoningTextItem,
    ReasoningTextKind, ThinkingSummary, ToolCallItem, ToolChoice, ToolDefinition,
    ToolResponseHeader, ToolResultStatus, ToolType,
};
use tau_provider::{StreamRepetitionGuard, StreamRepetitionKey};

const DEFAULT_CONTEXT_WINDOW: u64 = 128_000;
const LOG_TARGET: &str = "provider-chat-completions";
const PROGRESS_METADATA_MIN_INTERVAL: Duration = Duration::from_secs(1);
/// Default Chat Completions output-token cap Tau sends when no
/// provider-specific override is set.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 8192;
const EMPTY_RESPONSE_MAX_RETRIES: usize = 10;

/// One Chat Completions-compatible provider entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatCompletionsProvider {
    /// Base URL without `/chat/completions`, e.g. `https://api.openai.com/v1`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub base_url: String,
    /// Bearer token sent in the `Authorization` header. Empty for local or
    /// otherwise keyless providers.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api_key: String,
    /// Model ids to publish under this provider namespace.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ChatCompletionsModel>,
    /// Provider-wide model capability tags applied to every published model.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<ModelTag>,
    /// Maximum output tokens requested from the upstream provider.
    ///
    /// Chat Completions servers often have small server-side defaults when the
    /// client omits this field. Set to `0` to omit Tau's automatic cap and rely
    /// on provider defaults or `extra_body` overrides.
    #[serde(
        default = "default_max_output_tokens",
        skip_serializing_if = "is_default_max_output_tokens"
    )]
    pub max_output_tokens: u32,
    /// Extra JSON fields merged into each Chat Completions request body.
    ///
    /// Local and OpenAI-compatible servers use non-standard knobs for reasoning
    /// (`chat_template_kwargs`, `reasoning`, `enable_thinking`, etc.). Keeping
    /// this map provider-scoped lets users opt into those fields without Tau
    /// needing a compatibility switch for every backend.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra_body: BTreeMap<String, serde_json::Value>,
    /// Explicit provider compatibility switches.
    #[serde(default)]
    pub compat: ChatCompletionsCompat,
}

/// One model published by a Chat Completions-compatible provider.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatCompletionsModel {
    /// Upstream model id sent in the `model` request field.
    pub id: ModelName,
    /// Optional UI display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Context window size surfaced to the harness.
    #[serde(default = "default_context_window")]
    pub context_window: u64,
    /// Optional model-specific compatibility overrides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compat: Option<ChatCompletionsCompat>,
    /// Model-specific capability tags added to the provider-wide tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<ModelTag>,
}

/// Compatibility switches for OpenAI-compatible Chat Completions APIs.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatCompletionsCompat {
    /// Whether to send `stream_options: { include_usage: true }`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub stream_options: bool,
    /// Whether to send `parallel_tool_calls` when tools are declared.
    #[serde(default, skip_serializing_if = "is_false")]
    pub parallel_tool_calls: bool,
    /// Whether to send OpenAI's `prompt_cache_key` field.
    #[serde(default, skip_serializing_if = "is_false")]
    pub prompt_cache_key: bool,
    /// Whether to send `reasoning_effort`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub reasoning_effort: bool,
    /// Whether to use `max_completion_tokens` for future output caps.
    #[serde(default, skip_serializing_if = "is_false")]
    pub max_completion_tokens: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

const fn default_context_window() -> u64 {
    DEFAULT_CONTEXT_WINDOW
}

const fn default_max_output_tokens() -> u32 {
    DEFAULT_MAX_OUTPUT_TOKENS
}

fn is_default_max_output_tokens(value: &u32) -> bool {
    *value == DEFAULT_MAX_OUTPUT_TOKENS
}

impl Default for ChatCompletionsProvider {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            api_key: String::new(),
            models: Vec::new(),
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            extra_body: BTreeMap::new(),
            tags: Vec::new(),
            compat: ChatCompletionsCompat::default(),
        }
    }
}

impl ChatCompletionsCompat {
    /// Compatibility switches for OpenAI's public Chat Completions API.
    #[must_use]
    pub const fn openai_defaults() -> Self {
        Self {
            stream_options: true,
            parallel_tool_calls: true,
            prompt_cache_key: true,
            reasoning_effort: true,
            max_completion_tokens: true,
        }
    }
}

fn run_prompt<W: Write>(
    agent_prompt_id: &AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    mut provider: ResolvedProvider,
    model: ChatCompletionsModel,
    debug_provider_requests: bool,
    writer: &mut PeerOutputWriter<W>,
) -> ProviderResponseFinished {
    if let Some(model_compat) = model.compat {
        provider.compat = model_compat;
    }
    let mut empty_response_retries = 0_usize;
    loop {
        let result = {
            let mut delta_emitter = StreamDeltaEmitter::default();
            let mut progress_emitter = ProviderProgressEmitter::new(Instant::now());
            let mut on_update = |state: &StreamState| {
                let deltas = delta_emitter.deltas(state);
                let progress = progress_emitter
                    .progress_for_update(state.streaming_progress(), Instant::now());
                if deltas.is_empty() && progress.is_none() {
                    return;
                }
                let _ = writer.write_message(&HarnessInputMessage::emit(
                    Event::ProviderResponseUpdated(ProviderResponseUpdated {
                        agent_prompt_id: agent_prompt_id.clone(),
                        agent_id: prompt.agent_id.clone(),
                        deltas,
                        compaction: None,
                        status: None,
                        progress,
                        originator: prompt.originator.clone(),
                    }),
                ));
                let _ = writer.flush();
            };
            chat_completions_stream(
                &provider,
                &model,
                prompt,
                debug_provider_requests,
                &mut on_update,
            )
        };
        match result {
            Ok(state) => return finish_success(agent_prompt_id, prompt, &provider, state),
            Err(LlmError::EmptyResponse) if empty_response_retries < EMPTY_RESPONSE_MAX_RETRIES => {
                empty_response_retries += 1;
                emit_empty_response_retry_update(
                    agent_prompt_id,
                    prompt,
                    empty_response_retries,
                    writer,
                );
            }
            Err(error @ LlmError::RepetitionDetected(_)) => {
                let LlmError::RepetitionDetected(repetition) = &error else {
                    unreachable!()
                };
                emit_repetition_detected_update(agent_prompt_id, prompt, repetition, writer);
                return finish_error(agent_prompt_id, prompt, &provider, error);
            }
            Err(error) => return finish_error(agent_prompt_id, prompt, &provider, error),
        }
    }
}

fn emit_empty_response_retry_update<W: Write>(
    agent_prompt_id: &AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    retry: usize,
    writer: &mut PeerOutputWriter<W>,
) {
    let text = format!(
        "provider returned an empty response; retrying ({retry}/{EMPTY_RESPONSE_MAX_RETRIES})"
    );
    let _ = writer.write_message(&HarnessInputMessage::emit(Event::ProviderResponseUpdated(
        ProviderResponseUpdated {
            agent_prompt_id: agent_prompt_id.clone(),
            agent_id: prompt.agent_id.clone(),
            deltas: Vec::new(),
            compaction: None,
            status: Some(ProviderResponseStatusUpdate {
                text,
                clear_response: true,
            }),
            progress: None,
            originator: prompt.originator.clone(),
        },
    )));
    let _ = writer.flush();
}

fn emit_repetition_detected_update<W: Write>(
    agent_prompt_id: &AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    repetition: &tau_provider::StreamRepetition,
    writer: &mut PeerOutputWriter<W>,
) {
    let text = bounded_provider_error(&format!(
        "provider stream repetition detected; aborting response ({repetition})"
    ));
    let _ = writer.write_message(&HarnessInputMessage::emit(Event::ProviderResponseUpdated(
        ProviderResponseUpdated {
            agent_prompt_id: agent_prompt_id.clone(),
            agent_id: prompt.agent_id.clone(),
            deltas: Vec::new(),
            compaction: None,
            status: Some(ProviderResponseStatusUpdate {
                text,
                clear_response: true,
            }),
            progress: None,
            originator: prompt.originator.clone(),
        },
    )));
    let _ = writer.flush();
}

/// Runs one prompt against a registered Chat Completions-compatible provider
/// profile.
pub fn run_prompt_for_provider<W: Write>(
    agent_prompt_id: &AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    provider: &ChatCompletionsProvider,
    model: &ChatCompletionsModel,
    debug_provider_requests: bool,
    writer: &mut PeerOutputWriter<W>,
) -> ProviderResponseFinished {
    run_prompt(
        agent_prompt_id,
        prompt,
        ResolvedProvider {
            base_url: provider.base_url.clone(),
            api_key: provider.api_key.clone(),
            max_output_tokens: provider.max_output_tokens,
            extra_body: provider.extra_body.clone(),
            compat: provider.compat,
        },
        model.clone(),
        debug_provider_requests,
        writer,
    )
}

#[derive(Clone)]
struct ResolvedProvider {
    base_url: String,
    api_key: String,
    max_output_tokens: u32,
    extra_body: BTreeMap<String, serde_json::Value>,
    compat: ChatCompletionsCompat,
}

/// Returns model publication records for one Chat Completions-compatible
/// provider profile.
pub fn models_for_provider(
    provider_name: &ProviderName,
    provider: &ChatCompletionsProvider,
) -> Vec<ProviderModelInfo> {
    provider
        .models
        .iter()
        .map(|model| ProviderModelInfo {
            id: ModelId::new(provider_name.clone(), model.id.clone()),
            display_name: model.display_name.clone(),
            tags: merged_model_tags(&provider.tags, &model.tags),
            default_affinity: 0,
            context_window: model.context_window,
            efforts: model_efforts(model.compat.unwrap_or(provider.compat)),
            verbosities: vec![tau_proto::Verbosity::Medium],
            thinking_summaries: vec![ThinkingSummary::Off],
            supports_compaction: false,
        })
        .collect()
}

fn merged_model_tags(provider_tags: &[ModelTag], model_tags: &[ModelTag]) -> Vec<ModelTag> {
    let mut tags = provider_tags.to_vec();
    for tag in model_tags {
        if !tags.iter().any(|existing| existing == tag) {
            tags.push(tag.clone());
        }
    }
    tags
}

fn model_efforts(compat: ChatCompletionsCompat) -> Vec<tau_proto::Effort> {
    if compat.reasoning_effort {
        vec![
            tau_proto::Effort::Off,
            tau_proto::Effort::Minimal,
            tau_proto::Effort::Low,
            tau_proto::Effort::Medium,
            tau_proto::Effort::High,
            tau_proto::Effort::XHigh,
        ]
    } else {
        vec![tau_proto::Effort::Off]
    }
}

#[derive(Debug)]
enum LlmError {
    EmptyResponse,
    Http(Box<ureq::Error>),
    HttpStatus(u16, String),
    Io(std::io::Error),
    Json(serde_json::Error),
    RepetitionDetected(tau_provider::StreamRepetition),
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyResponse => write!(f, "provider returned an empty response"),
            Self::Http(error) => write!(f, "HTTP error: {error}"),
            Self::HttpStatus(code, body) => write!(f, "HTTP {code}: {body}"),
            Self::Io(error) => write!(f, "I/O error: {error}"),
            Self::Json(error) => write!(f, "JSON error: {error}"),
            Self::RepetitionDetected(repetition) => write!(f, "{repetition}"),
        }
    }
}

struct StreamState {
    text: String,
    thinking: String,
    output_items: Vec<OutputItemAccumulator>,
    pending_content: String,
    in_think_tag: bool,
    tool_call_output_indices: HashMap<usize, usize>,
    input_tokens: Option<u64>,
    cached_tokens: Option<u64>,
    output_tokens: Option<u64>,
    stop_reason: ProviderStopReason,
    repetition_guard: StreamRepetitionGuard,
}

/// Tracks displayable text already emitted on transient response updates.
#[derive(Default)]
struct StreamDeltaEmitter {
    /// Assistant message text already emitted per output item.
    emitted_messages: HashMap<usize, String>,
    /// Reasoning text already emitted per output item.
    emitted_reasoning: HashMap<usize, String>,
}

impl StreamDeltaEmitter {
    /// Returns only newly appended assistant/reasoning text since the last
    /// call.
    fn deltas(&mut self, state: &StreamState) -> Vec<ProviderResponseTextDelta> {
        let mut deltas = Vec::new();
        for (output_index, item) in state.output_items.iter().enumerate() {
            match item {
                OutputItemAccumulator::Message(text) => {
                    if let Some(delta) =
                        append_suffix(self.emitted_messages.entry(output_index).or_default(), text)
                    {
                        deltas.push(ProviderResponseTextDelta::Message {
                            output_index: output_index as u32,
                            text: delta,
                            phase: None,
                        });
                    }
                }
                OutputItemAccumulator::Reasoning(text) => {
                    if let Some(delta) = append_suffix(
                        self.emitted_reasoning.entry(output_index).or_default(),
                        text,
                    ) {
                        deltas.push(ProviderResponseTextDelta::ReasoningText {
                            output_index: output_index as u32,
                            kind: ReasoningTextKind::Full,
                            text: delta,
                        });
                    }
                }
                OutputItemAccumulator::ToolCall(_) => {}
            }
        }
        deltas
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

impl StreamState {
    fn new() -> Self {
        Self {
            text: String::new(),
            thinking: String::new(),
            output_items: Vec::new(),
            pending_content: String::new(),
            in_think_tag: false,
            tool_call_output_indices: HashMap::new(),
            input_tokens: None,
            cached_tokens: None,
            output_tokens: None,
            stop_reason: ProviderStopReason::EndTurn,
            repetition_guard: StreamRepetitionGuard::new(),
        }
    }

    fn output_items(&self) -> Vec<ContextItem> {
        self.output_items
            .iter()
            .filter_map(OutputItemAccumulator::context_item)
            .collect()
    }

    fn append_assistant_text_delta(&mut self, delta: &str) -> Result<(), LlmError> {
        if delta.is_empty() {
            return Ok(());
        }
        let output_index = match self.output_items.last() {
            Some(OutputItemAccumulator::Message(_)) => self.output_items.len() - 1,
            _ => self.output_items.len(),
        };
        if let Some(repetition) = self
            .repetition_guard
            .push_delta(StreamRepetitionKey::AssistantText { output_index }, delta)
        {
            return Err(LlmError::RepetitionDetected(repetition));
        }
        self.text.push_str(delta);
        if let Some(OutputItemAccumulator::Message(text)) = self.output_items.last_mut() {
            text.push_str(delta);
        } else {
            self.output_items
                .push(OutputItemAccumulator::Message(delta.to_owned()));
        }
        Ok(())
    }

    fn append_reasoning_delta(&mut self, delta: &str) -> Result<(), LlmError> {
        if delta.is_empty() {
            return Ok(());
        }
        let output_index = match self.output_items.last() {
            Some(OutputItemAccumulator::Reasoning(_)) => self.output_items.len() - 1,
            _ => self.output_items.len(),
        };
        if let Some(repetition) = self
            .repetition_guard
            .push_delta(StreamRepetitionKey::ReasoningText { output_index }, delta)
        {
            return Err(LlmError::RepetitionDetected(repetition));
        }
        self.thinking.push_str(delta);
        if let Some(OutputItemAccumulator::Reasoning(reasoning)) = self.output_items.last_mut() {
            reasoning.push_str(delta);
        } else {
            self.output_items
                .push(OutputItemAccumulator::Reasoning(delta.to_owned()));
        }
        Ok(())
    }

    fn append_tool_arguments_delta(
        &mut self,
        stream_index: usize,
        delta: &str,
    ) -> Result<(), LlmError> {
        let output_index = *self
            .tool_call_output_indices
            .get(&stream_index)
            .unwrap_or(&self.output_items.len());
        if let Some(repetition) = self.repetition_guard.push_delta(
            StreamRepetitionKey::FunctionCallArguments { output_index },
            delta,
        ) {
            return Err(LlmError::RepetitionDetected(repetition));
        }
        self.tool_call_at_mut(stream_index)
            .arguments
            .push_str(delta);
        Ok(())
    }

    fn tool_call_at_mut(&mut self, stream_index: usize) -> &mut ToolCallAccumulator {
        let output_index = *self
            .tool_call_output_indices
            .entry(stream_index)
            .or_insert_with(|| {
                let output_index = self.output_items.len();
                self.output_items.push(OutputItemAccumulator::ToolCall(
                    ToolCallAccumulator::default(),
                ));
                output_index
            });
        let OutputItemAccumulator::ToolCall(call) = &mut self.output_items[output_index] else {
            unreachable!("tool-call slot was just initialized");
        };
        call
    }

    fn usage(&self) -> Option<ProviderTokenUsage> {
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

    fn has_output_items(&self) -> bool {
        self.output_items.iter().any(|item| match item {
            OutputItemAccumulator::Message(text) => !text.is_empty(),
            OutputItemAccumulator::Reasoning(reasoning) => !reasoning.is_empty(),
            OutputItemAccumulator::ToolCall(call) => !call.name.is_empty(),
        })
    }

    fn is_empty_end_turn(&self) -> bool {
        self.stop_reason == ProviderStopReason::EndTurn && !self.has_output_items()
    }

    /// Returns content-free byte progress for provider-generated semantic
    /// output in the current response.
    fn streaming_progress(&self) -> Option<ProviderResponseProgressUpdate> {
        let mut total_bytes = 0_u64;
        let mut items = Vec::new();
        let mut omitted_items = 0_u64;
        for (output_index, item) in self.output_items.iter().enumerate() {
            let (kind, counter_end_bytes, label) = match item {
                OutputItemAccumulator::Message(text) => (
                    ProviderResponseProgressKind::AssistantText,
                    text.len() as u64,
                    None,
                ),
                OutputItemAccumulator::Reasoning(text) => (
                    ProviderResponseProgressKind::ReasoningText,
                    text.len() as u64,
                    None,
                ),
                OutputItemAccumulator::ToolCall(call) => (
                    ProviderResponseProgressKind::ToolArguments,
                    call.arguments.len() as u64,
                    bounded_progress_label(&call.name),
                ),
            };
            if counter_end_bytes == 0 {
                continue;
            }
            total_bytes = total_bytes.saturating_add(counter_end_bytes);
            if items.len() < 4 {
                items.push(ProviderResponseProgressItem {
                    output_index: output_index as u32,
                    kind,
                    counter_start_bytes: 0,
                    counter_end_bytes,
                    window_micros: 0,
                    label,
                });
            } else {
                omitted_items += 1;
            }
        }
        (total_bytes > 0).then_some(ProviderResponseProgressUpdate {
            total_counter_start_bytes: 0,
            total_counter_end_bytes: total_bytes,
            total_window_micros: 0,
            items,
            omitted_items,
        })
    }
}

fn bounded_progress_label(label: &str) -> Option<String> {
    const MAX_LABEL_CHARS: usize = 32;
    if label.is_empty() {
        return None;
    }
    Some(label.chars().take(MAX_LABEL_CHARS).collect())
}

/// Provider-side sampler that makes progress updates self-contained for UIs.
struct ProgressSampleState {
    /// Time at which the last emitted progress sample ended.
    last_sample_at: Instant,
    /// Last emitted end counter for each detailed progress item.
    last_counters: BTreeMap<(u32, ProviderResponseProgressKind), u64>,
    /// Last emitted aggregate end counter across all counted output items.
    last_total_counter: u64,
}

impl ProgressSampleState {
    fn new(started_at: Instant) -> Self {
        Self {
            last_sample_at: started_at,
            last_counters: BTreeMap::new(),
            last_total_counter: 0,
        }
    }

    fn with_sample_window(
        &mut self,
        mut progress: ProviderResponseProgressUpdate,
        now: Instant,
    ) -> ProviderResponseProgressUpdate {
        let window_micros = now
            .saturating_duration_since(self.last_sample_at)
            .as_micros()
            .max(1) as u64;
        progress.total_counter_start_bytes = self.last_total_counter;
        progress.total_window_micros = window_micros;
        for item in &mut progress.items {
            let key = (item.output_index, item.kind);
            item.counter_start_bytes = self
                .last_counters
                .get(&key)
                .copied()
                .unwrap_or(item.counter_start_bytes);
            item.window_micros = window_micros;
            self.last_counters.insert(key, item.counter_end_bytes);
        }
        self.last_total_counter = progress.total_counter_end_bytes;
        self.last_sample_at = now;
        progress
    }
}

fn progress_current_bytes(progress: Option<&ProviderResponseProgressUpdate>) -> Option<u64> {
    progress.map(|progress| progress.total_counter_end_bytes)
}

/// Decides when to emit provider progress and attaches sample-window counters.
struct ProviderProgressEmitter {
    /// Last aggregate byte counter emitted in a progress update.
    last_progress_bytes: Option<u64>,
    /// Last time progress metadata was emitted.
    last_progress_emit: Instant,
    /// Sampler that fills start counters and window durations.
    progress_sample: ProgressSampleState,
}

impl ProviderProgressEmitter {
    fn new(now: Instant) -> Self {
        let initial_sample_at = now - PROGRESS_METADATA_MIN_INTERVAL;
        Self {
            last_progress_bytes: None,
            last_progress_emit: initial_sample_at,
            progress_sample: ProgressSampleState::new(initial_sample_at),
        }
    }

    fn progress_for_update(
        &mut self,
        progress: Option<ProviderResponseProgressUpdate>,
        now: Instant,
    ) -> Option<ProviderResponseProgressUpdate> {
        let progress_bytes = progress_current_bytes(progress.as_ref());
        let progress_changed = progress_bytes != self.last_progress_bytes;
        let can_emit_progress = progress_changed
            && now.saturating_duration_since(self.last_progress_emit)
                >= PROGRESS_METADATA_MIN_INTERVAL;
        if !can_emit_progress {
            return None;
        }
        self.last_progress_emit = now;
        self.last_progress_bytes = progress_bytes;
        progress.map(|progress| self.progress_sample.with_sample_window(progress, now))
    }
}

enum OutputItemAccumulator {
    Message(String),
    Reasoning(String),
    ToolCall(ToolCallAccumulator),
}

impl OutputItemAccumulator {
    fn context_item(&self) -> Option<ContextItem> {
        match self {
            Self::Message(text) => (!text.is_empty()).then(|| assistant_text_item(text.clone())),
            Self::Reasoning(reasoning) => reasoning_text_context_item(reasoning),
            Self::ToolCall(call) => call.context_item(),
        }
    }
}

#[derive(Default)]
struct ToolCallAccumulator {
    id: String,
    name: String,
    arguments: String,
}

impl ToolCallAccumulator {
    fn context_item(&self) -> Option<ContextItem> {
        if self.name.is_empty() {
            return None;
        }
        Some(ContextItem::ToolCall(ToolCallItem {
            call_id: self.id.clone().into(),
            name: tau_proto::ToolName::new(self.name.clone()),
            tool_type: ToolType::Function,
            arguments: serde_json::from_str::<serde_json::Value>(&self.arguments)
                .map(|value| json_to_cbor(&value))
                .unwrap_or(tau_proto::CborValue::Null),
            raw_arguments_json: Some(self.arguments.clone()),
            responses_envelope: None,
        }))
    }
}

fn chat_completions_stream(
    provider: &ResolvedProvider,
    model: &ChatCompletionsModel,
    prompt: &tau_proto::AgentPromptCreated,
    debug_provider_requests: bool,
    on_update: &mut impl FnMut(&StreamState),
) -> Result<StreamState, LlmError> {
    let url = format!(
        "{}/chat/completions",
        provider.base_url.trim_end_matches('/')
    );
    let body = build_request(provider, model, prompt);
    let body_str = serde_json::to_string(&body).map_err(LlmError::Json)?;
    maybe_debug_write_provider_request(prompt, model, debug_provider_requests, &body);
    let mut request = tau_provider::oauth::proxy_agent()
        .post(&url)
        .content_type("application/json")
        .header("Accept", "text/event-stream");
    if !provider.api_key.trim().is_empty() {
        request = request.header("Authorization", format!("Bearer {}", provider.api_key));
    }
    let mut response = request
        .send(&body_str)
        .map_err(|error| LlmError::Http(Box::new(error)))?;
    if !response.status().is_success() {
        let code = response.status().as_u16();
        let body = response.body_mut().read_to_string().unwrap_or_default();
        maybe_debug_write_provider_http_error(prompt, model, debug_provider_requests, code, &body);
        return Err(LlmError::HttpStatus(code, body));
    }

    let mut state = StreamState::new();
    let mut raw_events = Vec::new();
    let reader = BufReader::new(response.body_mut().as_reader());
    for line in reader.lines() {
        let line = line.map_err(LlmError::Io)?;
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        if data == "[DONE]" {
            break;
        }
        let event: serde_json::Value = match serde_json::from_str(data) {
            Ok(event) => event,
            Err(_) => continue,
        };
        raw_events.push(event.clone());
        apply_event(&mut state, &event, on_update)?;
    }
    flush_pending_content(&mut state, on_update)?;
    maybe_debug_write_provider_response(
        prompt,
        model,
        debug_provider_requests,
        &state,
        &raw_events,
    );
    ensure_non_empty_end_turn(state)
}

fn ensure_non_empty_end_turn(state: StreamState) -> Result<StreamState, LlmError> {
    if state.is_empty_end_turn() {
        Err(LlmError::EmptyResponse)
    } else {
        Ok(state)
    }
}

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<serde_json::Value>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
    #[serde(flatten)]
    extra_body: BTreeMap<String, serde_json::Value>,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

fn build_request(
    provider: &ResolvedProvider,
    model: &ChatCompletionsModel,
    prompt: &tau_proto::AgentPromptCreated,
) -> ChatRequest {
    let mut messages = Vec::new();
    if !prompt.system_prompt.trim().is_empty() {
        messages.push(serde_json::json!({
            "role": "system",
            "content": prompt.system_prompt,
        }));
    }
    for block in &prompt.context.blocks {
        append_context_block(block, &mut messages);
    }
    let tools = prompt
        .tools
        .iter()
        .filter_map(convert_tool_definition)
        .collect::<Vec<_>>();
    let tool_choice = match (prompt.tool_choice, tools.is_empty()) {
        (ToolChoice::None, _) => Some("none"),
        (ToolChoice::Auto, false) => Some("auto"),
        (ToolChoice::Auto, true) => None,
    };
    let (max_tokens, max_completion_tokens) = output_token_cap_fields(provider);
    ChatRequest {
        model: model.id.as_str().to_owned(),
        messages,
        stream: true,
        stream_options: provider.compat.stream_options.then_some(StreamOptions {
            include_usage: true,
        }),
        parallel_tool_calls: (provider.compat.parallel_tool_calls && !tools.is_empty())
            .then_some(true),
        prompt_cache_key: provider
            .compat
            .prompt_cache_key
            .then(|| format!("tau:{}", prompt.agent_id)),
        reasoning_effort: provider
            .compat
            .reasoning_effort
            .then(|| effort_wire(prompt.model_params.effort)),
        max_tokens,
        max_completion_tokens,
        extra_body: provider.extra_body.clone(),
        tools,
        tool_choice,
    }
}

fn debug_provider_request_dir(session_id: &str, debug_provider_requests: bool) -> Option<PathBuf> {
    let state = tau_config::settings::state_dir()?;
    debug_provider_request_dir_in(&state, session_id, debug_provider_requests)
}

fn debug_provider_request_dir_in(
    state: &std::path::Path,
    session_id: &str,
    debug_provider_requests: bool,
) -> Option<PathBuf> {
    if !debug_provider_requests {
        return None;
    }
    let session_dir = tau_config::settings::sessions_dir_of(state).join(session_id);
    session_dir
        .is_dir()
        .then(|| session_dir.join("debug").join("provider-requests"))
}

fn debug_file_prefix(
    prompt: &tau_proto::AgentPromptCreated,
    model: &ChatCompletionsModel,
) -> serde_json::Value {
    serde_json::json!({
        "session_id": prompt.session_id,
        "agent_prompt_id": prompt.agent_prompt_id,
        "transport": "http-sse",
        "backend": "chat_completions",
        "model": model.id,
    })
}

fn debug_timestamp_micros() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
}

fn write_debug_json(
    prompt: &tau_proto::AgentPromptCreated,
    suffix: &str,
    debug_provider_requests: bool,
    metadata: &serde_json::Value,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let Some(dir) = debug_provider_request_dir(prompt.session_id.as_str(), debug_provider_requests)
    else {
        return Ok(());
    };
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!(
        "{}-{}-http-sse-{suffix}.json",
        debug_timestamp_micros(),
        prompt.agent_prompt_id,
    ));
    std::fs::write(path, serde_json::to_vec_pretty(metadata)?)?;
    Ok(())
}

fn maybe_debug_write_provider_request(
    prompt: &tau_proto::AgentPromptCreated,
    model: &ChatCompletionsModel,
    debug_provider_requests: bool,
    body: &ChatRequest,
) {
    let metadata = serde_json::json!({
        "session_id": prompt.session_id,
        "agent_prompt_id": prompt.agent_prompt_id,
        "transport": "http-sse",
        "backend": "chat_completions",
        "model": model.id,
        "context_item_count": prompt.context.flatten_iter().count(),
        "tool_count": prompt.tools.len(),
        "tool_choice": prompt.tool_choice,
        "body": body,
    });
    if let Err(error) = write_debug_json(prompt, "request", debug_provider_requests, &metadata) {
        tracing::warn!(
            target: LOG_TARGET,
            session_id = %prompt.session_id,
            agent_prompt_id = %prompt.agent_prompt_id,
            "failed to write chat completions provider request debug log: {error}",
        );
    }
}

fn maybe_debug_write_provider_response(
    prompt: &tau_proto::AgentPromptCreated,
    model: &ChatCompletionsModel,
    debug_provider_requests: bool,
    state: &StreamState,
    raw_events: &[serde_json::Value],
) {
    let mut metadata = debug_file_prefix(prompt, model);
    if let serde_json::Value::Object(map) = &mut metadata {
        map.insert(
            "usage".to_owned(),
            serde_json::to_value(state.usage()).unwrap_or_default(),
        );
        map.insert(
            "stop_reason".to_owned(),
            serde_json::to_value(state.stop_reason).unwrap_or_default(),
        );
        map.insert(
            "output_items".to_owned(),
            serde_json::to_value(state.output_items()).unwrap_or_default(),
        );
        map.insert(
            "raw_events".to_owned(),
            serde_json::Value::Array(raw_events.to_vec()),
        );
    }
    if let Err(error) = write_debug_json(prompt, "response", debug_provider_requests, &metadata) {
        tracing::warn!(
            target: LOG_TARGET,
            session_id = %prompt.session_id,
            agent_prompt_id = %prompt.agent_prompt_id,
            "failed to write chat completions provider response debug log: {error}",
        );
    }
}

fn maybe_debug_write_provider_http_error(
    prompt: &tau_proto::AgentPromptCreated,
    model: &ChatCompletionsModel,
    debug_provider_requests: bool,
    status: u16,
    body: &str,
) {
    let mut metadata = debug_file_prefix(prompt, model);
    if let serde_json::Value::Object(map) = &mut metadata {
        map.insert("http_status".to_owned(), serde_json::json!(status));
        map.insert("body".to_owned(), serde_json::json!(body));
    }
    if let Err(error) = write_debug_json(prompt, "response", debug_provider_requests, &metadata) {
        tracing::warn!(
            target: LOG_TARGET,
            session_id = %prompt.session_id,
            agent_prompt_id = %prompt.agent_prompt_id,
            "failed to write chat completions provider response debug log: {error}",
        );
    }
}

fn reasoning_text_context_item(reasoning: &str) -> Option<ContextItem> {
    (!reasoning.is_empty()).then(|| {
        ContextItem::ReasoningText(ReasoningTextItem {
            kind: ReasoningTextKind::Full,
            text: reasoning.to_owned(),
        })
    })
}

fn output_token_cap_fields(provider: &ResolvedProvider) -> (Option<u32>, Option<u32>) {
    if provider.max_output_tokens == 0
        || provider.extra_body.contains_key("max_tokens")
        || provider.extra_body.contains_key("max_completion_tokens")
    {
        return (None, None);
    }
    if provider.compat.max_completion_tokens {
        (None, Some(provider.max_output_tokens))
    } else {
        (Some(provider.max_output_tokens), None)
    }
}

fn append_context_block(block: &tau_proto::ContextBlock, messages: &mut Vec<serde_json::Value>) {
    match block {
        tau_proto::ContextBlock::UserInput(block) => {
            for item in &block.items {
                let ContextItem::Message(message) = item else {
                    continue;
                };
                let text = message_text(message);
                if text.is_empty() || message.role == ContextRole::User && text.trim().is_empty() {
                    continue;
                }
                messages.push(serde_json::json!({
                    "role": role_wire(&message.role),
                    "content": text,
                }));
            }
        }
        tau_proto::ContextBlock::AssistantResponse(block) => {
            let mut reasoning = String::new();
            let mut text = String::new();
            let mut tool_calls = Vec::new();
            for item in &block.output_items {
                match item {
                    ContextItem::ReasoningText(item) if item.kind == ReasoningTextKind::Full => {
                        reasoning.push_str(&item.text);
                    }
                    ContextItem::ReasoningText(_) => {}
                    ContextItem::Reasoning(item) => {
                        if let Some(part) = chat_completions_reasoning_text(item) {
                            reasoning.push_str(&part);
                        }
                    }
                    ContextItem::Message(message) if message.role == ContextRole::Assistant => {
                        text.push_str(&message_text(message));
                    }
                    ContextItem::ToolCall(call) => {
                        tool_calls.push(serde_json::json!({
                            "id": call.call_id,
                            "type": "function",
                            "function": {
                                "name": call.name,
                                "arguments": function_call_arguments_json(call),
                            }
                        }));
                    }
                    ContextItem::Message(_)
                    | ContextItem::ToolResult(_)
                    | ContextItem::CompactionTrigger
                    | ContextItem::Compaction(_)
                    | ContextItem::UnknownProviderItem(_) => {}
                }
            }
            if text.is_empty() && reasoning.is_empty() && tool_calls.is_empty() {
                return;
            }
            #[derive(Serialize)]
            struct AssistantReplayMessage {
                role: &'static str,
                content: Option<String>,
                #[serde(skip_serializing_if = "Option::is_none")]
                reasoning_content: Option<String>,
                #[serde(skip_serializing_if = "Vec::is_empty")]
                tool_calls: Vec<serde_json::Value>,
            }

            messages.push(
                serde_json::to_value(AssistantReplayMessage {
                    role: "assistant",
                    content: (!text.is_empty()).then_some(text),
                    reasoning_content: (!reasoning.is_empty()).then_some(reasoning),
                    tool_calls,
                })
                .expect("assistant replay message serializes"),
            );
        }
        tau_proto::ContextBlock::ToolResults(block) => {
            for result in &block.items {
                messages.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": result.call_id,
                    "content": tool_result_text(result.status.clone(), &result.output),
                }));
            }
        }
    }
}

fn function_call_arguments_json(call: &ToolCallItem) -> String {
    call.raw_arguments_json.clone().unwrap_or_else(|| {
        serde_json::to_string(&cbor_to_json(&call.arguments)).unwrap_or_default()
    })
}

fn chat_completions_reasoning_text(item: &OpaqueProviderItem) -> Option<String> {
    let value = cbor_to_json(&item.value);
    if value.get("type").and_then(|value| value.as_str()) != Some("chat_completions_reasoning") {
        return None;
    }
    value
        .get("reasoning_content")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn message_text(message: &tau_proto::MessageItem) -> String {
    let mut text = String::new();
    for part in &message.content {
        match part {
            ContentPart::Text { text: part } => text.push_str(part),
        }
    }
    text
}

fn role_wire(role: &ContextRole) -> &'static str {
    match role {
        ContextRole::System => "system",
        ContextRole::Developer => "system",
        ContextRole::User => "user",
        ContextRole::Assistant => "assistant",
    }
}

fn tool_result_text(status: ToolResultStatus, output: &tau_proto::ToolResponse) -> String {
    match status {
        ToolResultStatus::Success => output.render(),
        ToolResultStatus::Error { message } => {
            let mut response = output.clone();
            response.headers.insert(
                0,
                ToolResponseHeader {
                    key: "error".to_owned(),
                    value: message,
                },
            );
            response.render()
        }
        ToolResultStatus::Cancelled { reason } => tau_proto::ToolResponse {
            raw: tau_proto::CborValue::Null,
            headers: vec![ToolResponseHeader {
                key: "cancelled".to_owned(),
                value: reason,
            }],
            body: String::new(),
        }
        .render(),
    }
}

fn convert_tool_definition(tool: &ToolDefinition) -> Option<serde_json::Value> {
    if tool.tool_type != ToolType::Function {
        return None;
    }
    Some(serde_json::json!({
        "type": "function",
        "function": {
            "name": tool.model_visible_name.as_ref().unwrap_or(&tool.name),
            "description": tool.description,
            "parameters": tool.parameters,
        }
    }))
}

fn apply_event(
    state: &mut StreamState,
    event: &serde_json::Value,
    on_update: &mut impl FnMut(&StreamState),
) -> Result<(), LlmError> {
    if let Some(usage) = event.get("usage") {
        capture_usage(state, usage);
    }
    if apply_stream_error(state, event)? {
        on_update(state);
        return Ok(());
    }
    let Some(choice) = first_stream_choice(event) else {
        return Ok(());
    };
    let delta = &choice["delta"];
    if apply_text_delta(state, delta)? {
        on_update(state);
    }
    if apply_tool_call_deltas(state, delta)? {
        on_update(state);
    }
    apply_finish_reason(state, choice);
    Ok(())
}

fn apply_stream_error(
    state: &mut StreamState,
    event: &serde_json::Value,
) -> Result<bool, LlmError> {
    let Some(error) = event.get("error") else {
        return Ok(false);
    };
    let Some(message) = error.get("message").and_then(|m| m.as_str()) else {
        return Ok(false);
    };
    let mut text = String::new();
    if !state.text.is_empty() {
        text.push_str("\n\n");
    }
    text.push_str(&format!("[OpenRouter Stream Error: {message}]"));
    state.append_assistant_text_delta(&text)?;
    state.stop_reason = ProviderStopReason::Error;
    Ok(true)
}

fn first_stream_choice(event: &serde_json::Value) -> Option<&serde_json::Value> {
    event["choices"]
        .as_array()
        .and_then(|choices| choices.first())
}

fn apply_text_delta(state: &mut StreamState, delta: &serde_json::Value) -> Result<bool, LlmError> {
    let mut changed = false;
    for key in ["reasoning_content", "reasoning", "thinking"] {
        if let Some(reasoning) = non_empty_str(&delta[key]) {
            state.append_reasoning_delta(reasoning)?;
            changed = true;
        }
    }
    if let Some(content) = non_empty_str(&delta["content"]) {
        changed |= append_content_delta(state, content)?;
    }
    Ok(changed)
}

fn apply_tool_call_deltas(
    state: &mut StreamState,
    delta: &serde_json::Value,
) -> Result<bool, LlmError> {
    let Some(tool_calls) = delta["tool_calls"].as_array() else {
        return Ok(false);
    };
    let mut changed = false;
    for tool_call in tool_calls {
        changed |= apply_tool_call_delta(state, tool_call)?;
    }
    Ok(changed)
}

fn apply_tool_call_delta(
    state: &mut StreamState,
    tool_call: &serde_json::Value,
) -> Result<bool, LlmError> {
    let index = tool_call["index"].as_u64().unwrap_or(0) as usize;
    let function = &tool_call["function"];
    let mut changed = update_tool_call_metadata(state, index, tool_call, function);
    if let Some(arguments) = function["arguments"].as_str() {
        state.append_tool_arguments_delta(index, arguments)?;
        changed = true;
    }
    Ok(changed)
}

fn update_tool_call_metadata(
    state: &mut StreamState,
    index: usize,
    tool_call: &serde_json::Value,
    function: &serde_json::Value,
) -> bool {
    let entry = state.tool_call_at_mut(index);
    let mut changed = false;
    if let Some(id) = non_empty_str(&tool_call["id"]) {
        entry.id = id.to_owned();
        changed = true;
    }
    if let Some(name) = non_empty_str(&function["name"]) {
        entry.name = name.to_owned();
        changed = true;
    }
    changed
}

fn non_empty_str(value: &serde_json::Value) -> Option<&str> {
    value.as_str().filter(|value| !value.is_empty())
}

fn apply_finish_reason(state: &mut StreamState, choice: &serde_json::Value) {
    match choice["finish_reason"].as_str() {
        Some("tool_calls") => state.stop_reason = ProviderStopReason::ToolCalls,
        Some("stop") => state.stop_reason = ProviderStopReason::EndTurn,
        Some("length") => state.stop_reason = ProviderStopReason::Length,
        _ => {}
    }
}

fn append_content_delta(state: &mut StreamState, content: &str) -> Result<bool, LlmError> {
    state.pending_content.push_str(content);
    let mut changed = false;
    loop {
        if state.pending_content.is_empty() {
            return Ok(changed);
        }
        if state.in_think_tag {
            if let Some(index) = state.pending_content.find("</think>") {
                let reasoning = state.pending_content[..index].to_owned();
                state.append_reasoning_delta(&reasoning)?;
                state.pending_content.drain(..index + "</think>".len());
                state.in_think_tag = false;
                changed = true;
                continue;
            }
            let keep = partial_tag_suffix_len(&state.pending_content, "</think>");
            let emit_len = state.pending_content.len() - keep;
            if emit_len == 0 {
                return Ok(changed);
            }
            let reasoning = state.pending_content[..emit_len].to_owned();
            state.append_reasoning_delta(&reasoning)?;
            state.pending_content.drain(..emit_len);
            return Ok(true);
        }

        if let Some(index) = state.pending_content.find("<think>") {
            let text = state.pending_content[..index].to_owned();
            state.append_assistant_text_delta(&text)?;
            state.pending_content.drain(..index + "<think>".len());
            state.in_think_tag = true;
            changed = true;
            continue;
        }
        let keep = partial_tag_suffix_len(&state.pending_content, "<think>");
        let emit_len = state.pending_content.len() - keep;
        if emit_len == 0 {
            return Ok(changed);
        }
        let text = state.pending_content[..emit_len].to_owned();
        state.append_assistant_text_delta(&text)?;
        state.pending_content.drain(..emit_len);
        return Ok(true);
    }
}

fn partial_tag_suffix_len(text: &str, tag: &str) -> usize {
    let mut keep = 0;
    for len in 1..tag.len() {
        if text.ends_with(&tag[..len]) {
            keep = len;
        }
    }
    keep
}

fn flush_pending_content(
    state: &mut StreamState,
    on_update: &mut impl FnMut(&StreamState),
) -> Result<(), LlmError> {
    if state.pending_content.is_empty() {
        return Ok(());
    }
    if state.in_think_tag {
        let reasoning = state.pending_content.clone();
        state.append_reasoning_delta(&reasoning)?;
    } else {
        let text = state.pending_content.clone();
        state.append_assistant_text_delta(&text)?;
    }
    state.pending_content.clear();
    on_update(state);
    Ok(())
}

fn capture_usage(state: &mut StreamState, usage: &serde_json::Value) {
    state.input_tokens = usage["prompt_tokens"].as_u64();
    state.output_tokens = usage["completion_tokens"].as_u64();
    state.cached_tokens = usage["prompt_tokens_details"]["cached_tokens"].as_u64();
}

fn finish_success(
    agent_prompt_id: &AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    provider: &ResolvedProvider,
    state: StreamState,
) -> ProviderResponseFinished {
    ProviderResponseFinished {
        agent_prompt_id: agent_prompt_id.clone(),
        agent_id: prompt.agent_id.clone(),
        output_items: state.output_items(),
        stop_reason: state.stop_reason,
        error: None,
        originator: prompt.originator.clone(),
        usage: state.usage(),
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: Some(backend_descriptor(provider)),
        provider_response_id: None,
        ws_pool_delta: None,
    }
}

fn finish_error(
    agent_prompt_id: &AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    provider: &ResolvedProvider,
    error: LlmError,
) -> ProviderResponseFinished {
    ProviderResponseFinished {
        agent_prompt_id: agent_prompt_id.clone(),
        agent_id: prompt.agent_id.clone(),
        output_items: Vec::new(),
        stop_reason: match &error {
            LlmError::RepetitionDetected(_) => ProviderStopReason::RepetitionDetected,
            _ => ProviderStopReason::Error,
        },
        error: Some(bounded_provider_error(&format!("LLM error: {error}"))),
        originator: prompt.originator.clone(),
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: Some(backend_descriptor(provider)),
        provider_response_id: None,
        ws_pool_delta: None,
    }
}

fn bounded_provider_error(text: &str) -> String {
    const MAX_CHARS: usize = 512;
    let mut out = text.chars().take(MAX_CHARS).collect::<String>();
    if text.chars().nth(MAX_CHARS).is_some() {
        out.push('…');
    }
    out
}

fn backend_descriptor(provider: &ResolvedProvider) -> ProviderBackend {
    ProviderBackend {
        kind: ProviderBackendKind::ChatCompletions,
        base_url: provider.base_url.clone(),
        transport: ProviderBackendTransport::HttpSse,
        stale_chain_fallback: false,
    }
}

fn assistant_text_item(text: impl Into<String>) -> ContextItem {
    ContextItem::Message(tau_proto::MessageItem {
        role: ContextRole::Assistant,
        content: vec![ContentPart::Text { text: text.into() }],
        phase: None,
        responses_raw_json: None,
    })
}

fn effort_wire(effort: tau_proto::Effort) -> &'static str {
    match effort {
        tau_proto::Effort::Off => "none",
        tau_proto::Effort::Minimal => "minimal",
        tau_proto::Effort::Low => "low",
        tau_proto::Effort::Medium => "medium",
        tau_proto::Effort::High => "high",
        tau_proto::Effort::XHigh => "high",
    }
}

fn cbor_to_json(value: &tau_proto::CborValue) -> serde_json::Value {
    match value {
        tau_proto::CborValue::Null => serde_json::Value::Null,
        tau_proto::CborValue::Bool(v) => serde_json::Value::Bool(*v),
        tau_proto::CborValue::Integer(v) => {
            let n: i128 = (*v).into();
            serde_json::json!(n)
        }
        tau_proto::CborValue::Float(v) => serde_json::Number::from_f64(*v)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        tau_proto::CborValue::Text(v) => serde_json::Value::String(v.clone()),
        tau_proto::CborValue::Bytes(bytes) => serde_json::Value::Array(
            bytes
                .iter()
                .map(|byte| serde_json::Value::Number((*byte).into()))
                .collect(),
        ),
        tau_proto::CborValue::Array(items) => {
            serde_json::Value::Array(items.iter().map(cbor_to_json).collect())
        }
        tau_proto::CborValue::Map(entries) => {
            let mut map = serde_json::Map::new();
            for (key, value) in entries {
                let key = match key {
                    tau_proto::CborValue::Text(text) => text.clone(),
                    other => serde_json::to_string(&cbor_to_json(other)).unwrap_or_default(),
                };
                map.insert(key, cbor_to_json(value));
            }
            serde_json::Value::Object(map)
        }
        tau_proto::CborValue::Tag(_, inner) => cbor_to_json(inner),
        _ => serde_json::Value::Null,
    }
}

fn json_to_cbor(value: &serde_json::Value) -> tau_proto::CborValue {
    match value {
        serde_json::Value::Null => tau_proto::CborValue::Null,
        serde_json::Value::Bool(v) => tau_proto::CborValue::Bool(*v),
        serde_json::Value::Number(v) => {
            if let Some(n) = v.as_i64() {
                tau_proto::CborValue::Integer(n.into())
            } else if let Some(n) = v.as_u64() {
                tau_proto::CborValue::Integer(n.into())
            } else if let Some(n) = v.as_f64() {
                tau_proto::CborValue::Float(n)
            } else {
                tau_proto::CborValue::Null
            }
        }
        serde_json::Value::String(v) => tau_proto::CborValue::Text(v.clone()),
        serde_json::Value::Array(items) => {
            tau_proto::CborValue::Array(items.iter().map(json_to_cbor).collect())
        }
        serde_json::Value::Object(map) => tau_proto::CborValue::Map(
            map.iter()
                .map(|(key, value)| (tau_proto::CborValue::Text(key.clone()), json_to_cbor(value)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests;
