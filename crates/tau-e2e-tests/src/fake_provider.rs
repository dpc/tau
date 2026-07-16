//! Closed deterministic provider implementation used only by e2e tests.

#[cfg(test)]
mod tests;
mod validation;

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tau_client::{ClientError, ClientResult, ExtensionBuilder, TauExtension, TauExtensionRunner};
use tau_proto::{
    CborValue, ClientKind, ContentPart, ContextItem, ContextRecoveryDisposition, ContextRole,
    Effort, Event, EventName, InputModality, MessageItem, ProviderModelInfo, ProviderModelsUpdated,
    ProviderPromptSubmitted, ProviderResponseFinished, ProviderResponseTextDelta,
    ProviderResponseUpdated, ProviderStopReason, ThinkingSummary, ToolCallItem, ToolType,
    Verbosity,
};
use validation::{validate_v1, validate_v2};

use crate::scenario::{FAKE_MODEL_ID, ScenarioActionV2, ScenarioTurnV1, ScenarioV1, ScenarioV2};

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
    scenario: ScenarioConfig,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ScenarioConfig {
    /// Phase-one global FIFO grammar.
    V1(ScenarioV1),
    /// Phase-two independently correlated lane grammar.
    V2(ScenarioV2),
}

/// Mutable validated scenario, lane, hold, barrier, and checkpoint state.
#[derive(Default)]
struct FakeState {
    /// Validated configured scenario.
    scenario: Option<ScenarioConfig>,
    /// Next exact turn index.
    next_turn: usize,
    /// Bounded observation trace.
    trace: Option<Arc<Mutex<File>>>,
    /// V2 lane selected for each live agent after its initial `ctx_id`.
    agent_lanes: HashMap<tau_proto::AgentId, usize>,
    /// Independently persisted V2 cursors.
    lane_cursors: Vec<usize>,
    /// Provider-local durable cursor checkpoint.
    checkpoint: Option<PathBuf>,
    /// Exact pending cancellation holds.
    holds: HashMap<tau_proto::AgentPromptId, PendingHold>,
    /// Pending deterministic barrier participants.
    barriers: HashMap<String, Vec<BarrierParticipant>>,
}

struct PendingHold {
    /// Wakes the bounded worker when exact cancellation wins.
    cancel: mpsc::Sender<tau_proto::AgentPromptId>,
    /// Worker that owns the hard timeout and terminal response.
    join: thread::JoinHandle<HoldOutcome>,
    /// Causal completion signal sent before a timeout or cancellation terminal.
    completed: mpsc::Receiver<()>,
}

/// Causal terminal result returned by one bounded hold worker.
enum HoldOutcome {
    /// The exact prompt id carried by the cancellation request.
    Canceled(tau_proto::AgentPromptId),
    /// The hard deadline elapsed before cancellation.
    TimedOut,
    /// Extension shutdown dropped the cancellation channel.
    Shutdown,
}

struct BarrierParticipant {
    /// Dynamic prompt identity and routing copied into the terminal response.
    prompt: tau_proto::AgentPromptCreated,
    /// Lane-local response released when every participant arrives.
    response: String,
}

impl Drop for FakeState {
    fn drop(&mut self) {
        for (_, hold) in self.holds.drain() {
            drop(hold.cancel);
            let _ = hold.join.join();
        }
    }
}

/// Complete durable state needed for quiescent V2 restore.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CursorCheckpoint {
    /// Complete scenario identity that the cursors were derived from.
    scenario: ScenarioV2,
    /// Next action index for each scenario lane.
    cursors: Vec<usize>,
    /// Durable immutable agent-to-lane associations.
    agent_lanes: Vec<AgentLaneCheckpoint>,
}

/// One immutable durable agent-to-lane association.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AgentLaneCheckpoint {
    /// Durable harness agent identity.
    agent_id: tau_proto::AgentId,
    /// Index into the checkpoint scenario's lane vector.
    lane_index: usize,
}

/// Validated state loaded from a checkpoint or initialized empty.
struct RestoredScenarioState {
    /// Validated next action index for each lane.
    cursors: Vec<usize>,
    /// Validated immutable live-agent lane associations.
    agent_lanes: HashMap<tau_proto::AgentId, usize>,
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
                let scenario_name = cx.config.scenario.name();
                let mut trace = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open("fake-provider.trace")
                    .map_err(|error| ClientError::handler(format!("open trace: {error}")))?;
                writeln!(trace, "scenario={scenario_name} configured")
                    .map_err(|error| ClientError::handler(format!("write trace: {error}")))?;
                let checkpoint = cx.state_dir().map(|dir| dir.join("scenario-cursor.json"));
                let restored = cx.config.scenario.restore_state(checkpoint.as_deref())?;
                cx.state.scenario = Some(cx.config.scenario);
                cx.state.lane_cursors = restored.cursors;
                cx.state.agent_lanes = restored.agent_lanes;
                cx.state.checkpoint = checkpoint;
                cx.state.trace = Some(Arc::new(Mutex::new(trace)));
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
            .on_raw_live(
                tau_proto::EventSelector::Exact(EventName::UI_CANCEL_PROMPT),
                |cx| {
                    let Event::UiCancelPrompt(cancel) = cx.delivery.event() else {
                        return Ok(());
                    };
                    cx.state.handle_cancel(cancel, &cx.handle)
                },
            )
            .ready_message("deterministic fake provider ready");
    }
}

impl FakeConfig {
    /// Validates the selected versioned scenario grammar and resource limits.
    fn validate(&self) -> ClientResult<()> {
        match &self.scenario {
            ScenarioConfig::V1(scenario) => validate_v1(scenario),
            ScenarioConfig::V2(scenario) => validate_v2(scenario),
        }
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
        self.reap_finished_holds()?;
        if matches!(self.scenario, Some(ScenarioConfig::V2(_))) {
            return self.handle_v2_prompt(prompt, handle);
        }
        let index = self.next_turn;
        let (turn, turn_count) = self
            .scenario
            .as_ref()
            .ok_or_else(|| ClientError::handler("prompt arrived before valid configuration"))
            .map(|scenario| match scenario {
                ScenarioConfig::V1(scenario) => {
                    (scenario.turns.get(index).cloned(), scenario.turns.len())
                }
                ScenarioConfig::V2(_) => unreachable!("V2 handled above"),
            })?;
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

    fn handle_v2_prompt(
        &mut self,
        prompt: &tau_proto::AgentPromptCreated,
        handle: &tau_client::ClientHandle,
    ) -> ClientResult<()> {
        if prompt.model.provider.as_str() != "fake"
            || prompt.model.model.as_str() != "test"
            || !prompt.operation.is_inference()
        {
            return Err(self.mismatch(0, "model/operation mismatch"));
        }
        let lane_index = if let Some(lane) = self.agent_lanes.get(&prompt.agent_id) {
            *lane
        } else if let Some(ctx_id) = prompt.ctx_id.as_deref() {
            let scenario = self.v2()?;
            scenario
                .lanes
                .iter()
                .position(|lane| lane.ctx_id == ctx_id)
                .ok_or_else(|| {
                    ClientError::handler("scenario first mismatch: unknown lane ctx_id")
                })?
        } else {
            *self.agent_lanes.get(&prompt.agent_id).ok_or_else(|| {
                ClientError::handler("scenario first mismatch: continuation has no bound lane")
            })?
        };
        self.agent_lanes
            .entry(prompt.agent_id.clone())
            .or_insert(lane_index);
        let cursor = self.lane_cursors.get(lane_index).copied().unwrap_or(0);
        let (action, action_count, lane_id) = {
            let scenario = self.v2()?;
            let lane = &scenario.lanes[lane_index];
            (
                lane.actions.get(cursor).cloned(),
                lane.actions.len(),
                lane.ctx_id.clone(),
            )
        };
        let Some(action) = action else {
            return Err(ClientError::handler(format!(
                "scenario first mismatch: lane {lane_id} already consumed"
            )));
        };
        self.require_v2_user_text(prompt, action.user_text())?;
        self.trace(&format!(
            "lane={lane_id} action={cursor} prompt_id={}",
            prompt.agent_prompt_id
        ))?;
        handle.emit(Event::ProviderPromptSubmitted(ProviderPromptSubmitted {
            agent_prompt_id: prompt.agent_prompt_id.clone(),
            originator: prompt.originator.clone(),
        }))?;
        self.lane_cursors[lane_index] += 1;
        self.persist_cursors()?;
        self.trace(&format!(
            "lane={lane_id} action={cursor} matched remaining={}",
            action_count - self.lane_cursors[lane_index]
        ))?;

        match action {
            ScenarioActionV2::Text { response, .. } => {
                handle.emit(Event::ProviderResponseFinished(finished(
                    prompt,
                    vec![assistant_message(response)],
                    ProviderStopReason::EndTurn,
                )))
            }
            ScenarioActionV2::Error {
                failure_kind,
                error,
                ..
            } => {
                let mut terminal = finished(prompt, Vec::new(), ProviderStopReason::Error);
                terminal.error = Some(error);
                terminal.failure_kind = Some(failure_kind);
                handle.emit(Event::ProviderResponseFinished(terminal))
            }
            ScenarioActionV2::Disconnect { reason, .. } => {
                self.trace(&format!("deliberate_disconnect={reason}"))?;
                Err(ClientError::handler(format!(
                    "deliberate scenario disconnect: {reason}"
                )))
            }
            ScenarioActionV2::HoldUntilCancel { timeout_ms, .. } => {
                let (cancel, wait) = mpsc::channel();
                let (completion, completed) = mpsc::channel();
                let trace = self
                    .trace
                    .clone()
                    .ok_or_else(|| ClientError::handler("trace not configured"))?;
                let handle = handle.clone();
                let held_prompt = prompt.clone();
                let prompt_id = prompt.agent_prompt_id.clone();
                let join = thread::spawn(move || {
                    match wait.recv_timeout(Duration::from_millis(timeout_ms)) {
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            let mut terminal =
                                finished(&held_prompt, Vec::new(), ProviderStopReason::Error);
                            terminal.error = Some("deterministic hold timed out".to_owned());
                            terminal.failure_kind = Some(tau_proto::ProviderFailureKind::Unknown);
                            let _ = write_shared_trace(
                                &trace,
                                &format!("prompt_id={prompt_id} hold_timeout"),
                            );
                            let _ = completion.send(());
                            let _ = handle.emit(Event::ProviderResponseFinished(terminal));
                            HoldOutcome::TimedOut
                        }
                        Ok(canceled_by) => {
                            let _ = write_shared_trace(
                                &trace,
                                &format!(
                                    "prompt_id={prompt_id} hold_canceled canceled_by={canceled_by}"
                                ),
                            );
                            let mut terminal =
                                finished(&held_prompt, Vec::new(), ProviderStopReason::Error);
                            terminal.error = Some("(cancelled by harness)".to_owned());
                            terminal.failure_kind = Some(tau_proto::ProviderFailureKind::Unknown);
                            let _ = completion.send(());
                            let _ = handle.emit(Event::ProviderResponseFinished(terminal));
                            HoldOutcome::Canceled(canceled_by)
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => HoldOutcome::Shutdown,
                    }
                });
                self.holds.insert(
                    prompt.agent_prompt_id.clone(),
                    PendingHold {
                        cancel,
                        join,
                        completed,
                    },
                );
                Ok(())
            }
            ScenarioActionV2::BarrierText {
                barrier,
                participants,
                response,
                ..
            } => {
                let pending = self.barriers.entry(barrier.clone()).or_default();
                pending.push(BarrierParticipant {
                    prompt: prompt.clone(),
                    response,
                });
                if pending.len() > participants {
                    return Err(ClientError::handler("barrier over-subscribed"));
                }
                if pending.len() == participants {
                    let completed = self.barriers.remove(&barrier).unwrap_or_default();
                    for participant in completed {
                        handle.emit(Event::ProviderResponseFinished(finished(
                            &participant.prompt,
                            vec![assistant_message(participant.response)],
                            ProviderStopReason::EndTurn,
                        )))?;
                    }
                }
                Ok(())
            }
        }
    }

    fn handle_cancel(
        &mut self,
        cancel: &tau_proto::UiCancelPrompt,
        handle: &tau_client::ClientHandle,
    ) -> ClientResult<()> {
        let Some(prompt_id) = cancel.agent_prompt_id.as_ref() else {
            return Ok(());
        };
        let mut active = self
            .holds
            .keys()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        active.sort();
        let Some(hold) = self.holds.remove(prompt_id) else {
            return Ok(());
        };
        // A timeout may win the deadline race and drop its receiver first. That
        // is already a terminal outcome, not a provider protocol failure.
        let _ = hold.cancel.send(prompt_id.clone());
        let outcome = hold
            .join
            .join()
            .map_err(|_| ClientError::handler("hold cancellation worker panicked"))?;
        let HoldOutcome::Canceled(canceled_by) = outcome else {
            self.trace(&format!("prompt_id={prompt_id} cancel_after_timeout"))?;
            return Ok(());
        };
        if canceled_by != *prompt_id {
            return Err(ClientError::handler(
                "hold woke for a different cancellation identity",
            ));
        }
        handle.emit(Event::HarnessNotice(tau_proto::HarnessNotice {
            kind: "e2e_fake_provider.cancel_completed".to_owned(),
            message: format!(
                "e2e_fake_provider.cancel_completed {}",
                serde_json::to_string(&serde_json::json!({
                    "selected": prompt_id.to_string(),
                    "canceled_by": canceled_by.to_string(),
                    "active_before": active,
                }))
                .map_err(|error| ClientError::handler(error.to_string()))?
            ),
            level: tau_proto::NoticeLevel::Trace,
            always_show: false,
        }))?;
        Ok(())
    }

    fn reap_finished_holds(&mut self) -> ClientResult<()> {
        let finished = self
            .holds
            .iter()
            .filter_map(|(id, hold)| hold.completed.try_recv().ok().map(|()| id.clone()))
            .collect::<Vec<_>>();
        for id in finished {
            let Some(hold) = self.holds.remove(&id) else {
                continue;
            };
            let outcome = hold
                .join
                .join()
                .map_err(|_| ClientError::handler("hold timeout worker panicked"))?;
            if !matches!(outcome, HoldOutcome::TimedOut) {
                return Err(ClientError::handler(
                    "completed cancellation worker remained in hold map",
                ));
            }
            self.trace(&format!("prompt_id={id} hold_reaped"))?;
        }
        Ok(())
    }

    fn v2(&self) -> ClientResult<&ScenarioV2> {
        match self.scenario.as_ref() {
            Some(ScenarioConfig::V2(scenario)) => Ok(scenario),
            _ => Err(ClientError::handler("V2 scenario not configured")),
        }
    }

    fn require_v2_user_text(
        &mut self,
        prompt: &tau_proto::AgentPromptCreated,
        expected: &str,
    ) -> ClientResult<()> {
        self.require_user_text(0, prompt, expected)
    }

    fn persist_cursors(&self) -> ClientResult<()> {
        let Some(path) = &self.checkpoint else {
            return Ok(());
        };
        let parent = path
            .parent()
            .ok_or_else(|| ClientError::handler("invalid cursor path"))?;
        std::fs::create_dir_all(parent)
            .map_err(|error| ClientError::handler(format!("create cursor directory: {error}")))?;
        let tmp = path.with_extension("tmp");
        let scenario = self.v2()?;
        let checkpoint = CursorCheckpoint {
            scenario: scenario.clone(),
            cursors: self.lane_cursors.clone(),
            agent_lanes: self
                .agent_lanes
                .iter()
                .map(|(agent_id, lane_index)| AgentLaneCheckpoint {
                    agent_id: agent_id.clone(),
                    lane_index: *lane_index,
                })
                .collect(),
        };
        std::fs::write(
            &tmp,
            serde_json::to_vec(&checkpoint)
                .map_err(|error| ClientError::handler(error.to_string()))?,
        )
        .map_err(|error| ClientError::handler(format!("write cursor: {error}")))?;
        std::fs::rename(&tmp, path)
            .map_err(|error| ClientError::handler(format!("commit cursor: {error}")))
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
        let Some(file) = self.trace.as_ref() else {
            return Ok(());
        };
        write_shared_trace(file, message)
    }
}

impl ScenarioActionV2 {
    fn user_text(&self) -> &str {
        match self {
            Self::Text { user_text, .. }
            | Self::Error { user_text, .. }
            | Self::HoldUntilCancel { user_text, .. }
            | Self::Disconnect { user_text, .. }
            | Self::BarrierText { user_text, .. } => user_text,
        }
    }
}

impl ScenarioConfig {
    fn name(&self) -> &str {
        match self {
            Self::V1(scenario) => &scenario.name,
            Self::V2(scenario) => &scenario.name,
        }
    }

    fn restore_state(&self, path: Option<&std::path::Path>) -> ClientResult<RestoredScenarioState> {
        let Self::V2(scenario) = self else {
            return Ok(RestoredScenarioState {
                cursors: Vec::new(),
                agent_lanes: HashMap::new(),
            });
        };
        let Some(path) = path else {
            return Ok(RestoredScenarioState {
                cursors: vec![0; scenario.lanes.len()],
                agent_lanes: HashMap::new(),
            });
        };
        match std::fs::read(path) {
            Ok(bytes) => {
                let checkpoint: CursorCheckpoint = serde_json::from_slice(&bytes)
                    .map_err(|error| ClientError::handler(format!("decode cursor: {error}")))?;
                if checkpoint.scenario != *scenario
                    || checkpoint.cursors.len() != scenario.lanes.len()
                    || checkpoint
                        .cursors
                        .iter()
                        .zip(&scenario.lanes)
                        .any(|(cursor, lane)| *cursor > lane.actions.len())
                {
                    return Err(ClientError::handler(
                        "cursor checkpoint does not match configured lanes",
                    ));
                }
                let mut agent_lanes = HashMap::new();
                for binding in checkpoint.agent_lanes {
                    if binding.lane_index >= scenario.lanes.len()
                        || agent_lanes
                            .insert(binding.agent_id, binding.lane_index)
                            .is_some()
                    {
                        return Err(ClientError::handler(
                            "cursor checkpoint contains invalid agent lane bindings",
                        ));
                    }
                }
                Ok(RestoredScenarioState {
                    cursors: checkpoint.cursors,
                    agent_lanes,
                })
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(RestoredScenarioState {
                    cursors: vec![0; scenario.lanes.len()],
                    agent_lanes: HashMap::new(),
                })
            }
            Err(error) => Err(ClientError::handler(format!("read cursor: {error}"))),
        }
    }
}

fn write_shared_trace(trace: &Arc<Mutex<File>>, message: &str) -> ClientResult<()> {
    let bounded = bounded_trace_message(message);
    let mut file = trace
        .lock()
        .map_err(|_| ClientError::handler("trace lock poisoned"))?;
    writeln!(file, "{bounded}")
        .and_then(|()| file.flush())
        .map_err(|error| ClientError::handler(format!("write trace: {error}")))
}

fn bounded_trace_message(message: &str) -> &str {
    let mut end = message.len().min(1024);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    &message[..end]
}
