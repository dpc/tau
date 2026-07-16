//! Closed deterministic provider implementation used only by e2e tests.

#[cfg(test)]
mod tests;

use std::fs::{File, OpenOptions};
use std::io::Write as _;

use serde::Deserialize;
use tau_client::{ClientError, ClientResult, ExtensionBuilder, TauExtension, TauExtensionRunner};
use tau_proto::{
    CborValue, ClientKind, ContentPart, ContextItem, ContextRecoveryDisposition, ContextRole,
    Effort, Event, EventName, InputModality, MessageItem, ProviderModelInfo, ProviderModelsUpdated,
    ProviderPromptSubmitted, ProviderResponseFinished, ProviderResponseTextDelta,
    ProviderResponseUpdated, ProviderStopReason, ThinkingSummary, ToolCallItem, ToolType,
    Verbosity,
};

use crate::scenario::{FAKE_MODEL_ID, ScenarioTurnV1, ScenarioV1};

const MAX_TURNS: usize = 8;
const MAX_SCENARIO_BYTES: usize = 16 * 1024;
const MAX_DELTAS: usize = 8;

/// Runs the deterministic provider over standard input/output.
pub fn run_stdio() -> Result<(), Box<dyn std::error::Error>> {
    TauExtensionRunner::new(FakeProvider).run(
        std::io::stdin(),
        std::io::stdout(),
        FakeState::default(),
    )?;
    Ok(())
}

/// Tau-client declaration for the deterministic fake provider.
struct FakeProvider;

/// Strict startup configuration supplied by the fixture.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FakeConfig {
    /// Inline versioned scenario.
    scenario: ScenarioV1,
}

/// Mutable FIFO scenario state.
#[derive(Default)]
struct FakeState {
    /// Validated configured scenario.
    scenario: Option<ScenarioV1>,
    /// Next exact turn index.
    next_turn: usize,
    /// Bounded observation trace.
    trace: Option<File>,
}

impl TauExtension for FakeProvider {
    type State = FakeState;

    fn name(&self) -> &'static str {
        "tau-e2e-fake-provider"
    }

    fn kind(&self) -> ClientKind {
        ClientKind::Provider
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder
            .configure::<FakeConfig>(|cx| {
                cx.config.validate()?;
                let mut trace = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open("fake-provider.trace")
                    .map_err(|error| ClientError::handler(format!("create trace: {error}")))?;
                writeln!(trace, "scenario={} configured", cx.config.scenario.name)
                    .map_err(|error| ClientError::handler(format!("write trace: {error}")))?;
                cx.state.scenario = Some(cx.config.scenario);
                cx.state.trace = Some(trace);
                cx.handle
                    .emit(Event::ProviderModelsUpdated(model_snapshot()))?;
                Ok(())
            })
            .on_raw_routed_live(
                tau_proto::EventSelector::Exact(EventName::AGENT_PROMPT_CREATED),
                |cx| {
                    let handle = cx.handle.clone();
                    let Event::AgentPromptCreated(prompt) = cx.delivery.event() else {
                        return Ok(());
                    };
                    cx.state.handle_prompt(prompt, &handle)
                },
            )
            .ready_message("deterministic fake provider ready");
    }
}

impl FakeConfig {
    /// Validates the complete phase-one scenario grammar and resource limits.
    fn validate(&self) -> ClientResult<()> {
        if self.scenario.version != 1 {
            return Err(ClientError::handler("scenario version must be 1"));
        }
        if self.scenario.turns.is_empty() || self.scenario.turns.len() > MAX_TURNS {
            return Err(ClientError::handler("scenario must contain 1..=8 turns"));
        }
        let total_text = serde_json::to_vec(&self.scenario)
            .map_err(|error| ClientError::handler(error.to_string()))?
            .len();
        if total_text > MAX_SCENARIO_BYTES {
            return Err(ClientError::handler("scenario exceeds 16384 bytes"));
        }
        match self.scenario.turns.as_slice() {
            [
                ScenarioTurnV1::Text {
                    user_text: _,
                    deltas,
                    response,
                },
            ] if !deltas.is_empty()
                && deltas.len() <= MAX_DELTAS
                && deltas.concat() == *response => {}
            [
                ScenarioTurnV1::ToolCall {
                    user_text: _,
                    tool_name,
                    call_id,
                },
                ScenarioTurnV1::ToolResult {
                    call_id: result_id,
                    response: _,
                },
            ] if tool_name.as_str() == tau_ext_test_dummy::RESTART_TEST_DUMMY_TOOL_NAME
                && !call_id.is_empty()
                && call_id.len() <= 256
                && call_id == result_id => {}
            [
                ScenarioTurnV1::Text {
                    user_text: _,
                    deltas: _,
                    response: _,
                },
            ] => {
                return Err(ClientError::handler(
                    "text scenario requires 1..=8 deltas concatenating to the final response",
                ));
            }
            _ => {
                return Err(ClientError::handler(
                    "scenario grammar must be one text turn or one matching tool call/result pair",
                ));
            }
        }
        Ok(())
    }
}

fn model_snapshot() -> ProviderModelsUpdated {
    ProviderModelsUpdated {
        models: vec![ProviderModelInfo {
            id: FAKE_MODEL_ID.into(),
            display_name: Some("Deterministic test model".to_owned()),
            tags: Vec::new(),
            supported_tool_types: vec![ToolType::Function],
            input_modalities: vec![InputModality::Text],
            tool_result_modalities: Vec::new(),
            supports_parallel_tool_calls: false,
            default_affinity: 0,
            context_window: 16_384,
            efforts: vec![Effort::Off],
            verbosities: vec![Verbosity::Low],
            thinking_summaries: vec![ThinkingSummary::Off],
            supports_compaction: false,
            supports_standalone_compaction: false,
            standalone_compaction_threshold: None,
        }],
    }
}

impl FakeState {
    fn handle_prompt(
        &mut self,
        prompt: &tau_proto::AgentPromptCreated,
        handle: &tau_client::ClientHandle,
    ) -> ClientResult<()> {
        let index = self.next_turn;
        let (turn, turn_count) = self
            .scenario
            .as_ref()
            .ok_or_else(|| ClientError::handler("prompt arrived before valid configuration"))
            .map(|scenario| (scenario.turns.get(index).cloned(), scenario.turns.len()))?;
        let Some(turn) = turn else {
            return Err(self.mismatch(index, "unexpected prompt after scenario consumption"));
        };
        if prompt.model.provider.as_str() != "fake"
            || prompt.model.model.as_str() != "test"
            || !prompt.operation.is_inference()
        {
            return Err(self.mismatch(index, "model/operation mismatch"));
        }
        self.trace(&format!(
            "turn={index} prompt_id={}",
            prompt.agent_prompt_id
        ))?;
        handle.emit(Event::ProviderPromptSubmitted(ProviderPromptSubmitted {
            agent_prompt_id: prompt.agent_prompt_id.clone(),
            originator: prompt.originator.clone(),
        }))?;
        let terminal = match turn {
            ScenarioTurnV1::Text {
                user_text,
                deltas,
                response,
            } => {
                self.require_user_text(index, prompt, &user_text)?;
                for text in deltas {
                    handle.emit(Event::ProviderResponseUpdated(ProviderResponseUpdated {
                        agent_prompt_id: prompt.agent_prompt_id.clone(),
                        agent_id: prompt.agent_id.clone(),
                        deltas: vec![ProviderResponseTextDelta::Message {
                            output_index: 0,
                            text,
                            phase: None,
                        }],
                        compaction: None,
                        status: None,
                        response_stats: None,
                        originator: prompt.originator.clone(),
                    }))?;
                }
                Event::ProviderResponseFinished(finished(
                    prompt,
                    vec![assistant_message(response)],
                    ProviderStopReason::EndTurn,
                ))
            }
            ScenarioTurnV1::ToolCall {
                user_text,
                tool_name,
                call_id,
            } => {
                self.require_user_text(index, prompt, &user_text)?;
                let tool_names = prompt
                    .tools
                    .iter()
                    .map(|tool| tool.name.as_str())
                    .collect::<Vec<_>>();
                if tool_names != [tool_name.as_str()] {
                    return Err(self.mismatch(index, "tool snapshot mismatch"));
                }
                let expected_schema = serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false,
                });
                if prompt.tools[0].tool_type != ToolType::Function
                    || prompt.tools[0].parameters.as_ref() != Some(&expected_schema)
                {
                    return Err(self.mismatch(index, "tool schema projection mismatch"));
                }
                Event::ProviderResponseFinished(finished(
                    prompt,
                    vec![ContextItem::ToolCall(ToolCallItem {
                        call_id,
                        name: tool_name,
                        tool_type: ToolType::Function,
                        arguments: CborValue::Map(Vec::new()),
                        raw_arguments_json: Some("{}".to_owned()),
                        responses_envelope: None,
                    })],
                    ProviderStopReason::ToolCalls,
                ))
            }
            ScenarioTurnV1::ToolResult { call_id, response } => {
                let results = prompt
                    .context
                    .flatten_iter()
                    .filter_map(|item| match item {
                        ContextItem::ToolResult(result) => Some(result),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                if results.len() != 1
                    || results[0].call_id != call_id
                    || results[0].status != tau_proto::ToolResultStatus::Success
                {
                    return Err(self.mismatch(index, "tool result continuity mismatch"));
                }
                Event::ProviderResponseFinished(finished(
                    prompt,
                    vec![assistant_message(response)],
                    ProviderStopReason::EndTurn,
                ))
            }
        };
        self.next_turn += 1;
        self.trace(&format!(
            "turn={index} matched remaining={}",
            turn_count - self.next_turn
        ))?;
        handle.emit(terminal)
    }

    fn require_user_text(
        &mut self,
        index: usize,
        prompt: &tau_proto::AgentPromptCreated,
        expected: &str,
    ) -> ClientResult<()> {
        let actual = prompt
            .context
            .flatten()
            .into_iter()
            .rev()
            .find_map(|item| match item {
                ContextItem::Message(message) if message.role == ContextRole::User => Some(
                    message
                        .content
                        .into_iter()
                        .map(|part| match part {
                            ContentPart::Text { text } => text,
                        })
                        .collect::<String>(),
                ),
                _ => None,
            });
        if actual.as_deref() != Some(expected) {
            return Err(self.mismatch(index, "last user text mismatch"));
        }
        Ok(())
    }
}

fn assistant_message(text: String) -> ContextItem {
    ContextItem::Message(MessageItem {
        role: ContextRole::Assistant,
        content: vec![ContentPart::Text { text }],
        phase: None,
        responses_raw_json: None,
    })
}

fn finished(
    prompt: &tau_proto::AgentPromptCreated,
    output_items: Vec<ContextItem>,
    stop_reason: ProviderStopReason,
) -> ProviderResponseFinished {
    ProviderResponseFinished {
        agent_prompt_id: prompt.agent_prompt_id.clone(),
        agent_id: prompt.agent_id.clone(),
        output_items,
        stop_reason,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: ContextRecoveryDisposition::None,
        originator: prompt.originator.clone(),
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    }
}

impl FakeState {
    fn mismatch(&mut self, index: usize, detail: &str) -> ClientError {
        let detail = format!("scenario first mismatch at turn {index}: {detail}");
        let _ = self.trace(&detail);
        ClientError::handler(detail)
    }

    fn trace(&mut self, message: &str) -> ClientResult<()> {
        let Some(file) = self.trace.as_mut() else {
            return Ok(());
        };
        let bounded = bounded_trace_message(message);
        writeln!(file, "{bounded}")
            .and_then(|()| file.flush())
            .map_err(|error| ClientError::handler(format!("write trace: {error}")))
    }
}

fn bounded_trace_message(message: &str) -> &str {
    let mut end = message.len().min(1024);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    &message[..end]
}
