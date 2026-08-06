//! Closed deterministic provider implementation used only by e2e tests.

use std::{collections as path_std_collections, io as path_std_io};

#[cfg(test)]
mod tests;
mod validation;

use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tau_client::{ClientError, ClientResult, ExtensionBuilder, TauExtension, TauExtensionRunner};
use tau_proto::{
    AgentMessageReceived, CborValue, ClientKind, ContentPart, ContextItem,
    ContextRecoveryDisposition, ContextRole, Effort, Event, EventName, InputModality, MessageItem,
    ProviderModelInfo, ProviderModelsDeclared, ProviderPromptSubmitted, ProviderResponseFinished,
    ProviderResponseTextDelta, ProviderResponseUpdated, ProviderStopReason, ThinkingSummary,
    ToolCallItem, ToolName, ToolType, Verbosity,
};
use validation::{validate_v1, validate_v2};

use crate::scenario::{
    AgentWatchResultExpectationV2, FAKE_MODEL_ID, ScenarioActionV2, ScenarioTurnV1, ScenarioV1,
    ScenarioV2, WatchNotificationV2,
};

const MAX_TURNS: usize = 8;
const MAX_SCENARIO_BYTES: usize = 16 * 1024;
const MAX_CHECKPOINT_BYTES: u64 = 64 * 1024;
const MAX_DELTAS: usize = 8;
const MAX_AGENT_START_PAIRS: usize = 2;

/// Closed public-PTY scenarios allowed to bind their sole lane from the
/// harness-minted interactive prompt correlation.
///
/// See `SPEC-tau-e2e-deterministic-provider`.
const PUBLIC_PTY_DYNAMIC_LANE_SCENARIOS: &[&str] = &[
    "spawned-tau-cold-resume",
    "live-dual-pty-attach",
    "prompt-stdin-success",
    "prompt-stdin-provider-failure",
];

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

impl ScenarioConfig {
    /// Returns whether the closed scenario explicitly exercises standalone
    /// compaction, the fake's sole opt-in capability expansion.
    fn enables_standalone_compaction(&self) -> bool {
        matches!(
            self,
            Self::V2(scenario)
                if scenario.lanes.iter().flat_map(|lane| &lane.actions).any(|action| {
                    matches!(
                        action,
                        ScenarioActionV2::StandaloneCompaction { summary: _ }
                            | ScenarioActionV2::StandaloneCompactionError {
                                failure_kind: _,
                                error: _,
                            }
                            | ScenarioActionV2::StandaloneCompactionHold { timeout_ms: _ }
                    )
                })
        )
    }
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
    /// Non-initial model-visible watch deliveries awaiting one provider prompt.
    watch_notifications: HashMap<tau_proto::AgentId, VecDeque<AgentMessageReceived>>,
    /// Ordered child identities learned from exact successful `agent_start`
    /// results for each parent.
    child_agents: HashMap<tau_proto::AgentId, Vec<tau_proto::AgentId>>,
    /// Count of exact watch notifications consumed by a staged action.
    watch_progress: HashMap<tau_proto::AgentId, usize>,
    /// Admitted facts for the current two-chain watch action.
    watch_chain_progress: HashMap<tau_proto::AgentId, WatchChainProgress>,
    /// Live S6 repair-pair progress for the sole closed dummy call; `None`
    /// means the exact `provider.tool_error` has not arrived.
    repair_progress: Option<DummyRepairProgress>,
}

/// Correlation and phase for the sole live S6 repair pair.
struct DummyRepairProgress {
    /// Provider-authored call identity declared by the closed repair action.
    call_id: tau_proto::ToolCallId,
    /// Next exact live repair event accepted by the fake.
    phase: DummyRepairPhase,
}

/// Closed live-event ordering for one S6 repair pair.
enum DummyRepairPhase {
    /// The durable provider error arrived; the renderer error is next.
    AwaitingToolError,
    /// Both events arrived in exact order.
    Complete,
}

/// Progress through one two-fact causal chain.
#[derive(Clone, Copy, Default)]
enum WatchCausalChain {
    /// Neither fact has arrived.
    #[default]
    Pending,
    /// The predecessor arrived and the successor is now admissible.
    PredecessorSeen,
    /// Both facts arrived in causal order.
    Complete,
}

/// One semantic fact in the direct watch content chain.
enum WatchChainFact {
    /// Direct user prompt.
    Prompt,
    /// Final child response.
    Response,
}

/// Admission state for the prompt-before-response watch-notification chain.
#[derive(Clone, Copy, Default)]
struct WatchChainProgress {
    /// Prompt-before-response chain.
    prompt_response: WatchCausalChain,
}

impl WatchChainProgress {
    /// Admits one fact only when its own chain predecessor permits it.
    fn admit(&mut self, message: &AgentMessageReceived) -> Result<WatchChainFact, &'static str> {
        match message.kind {
            tau_proto::AgentMessageKind::WatchPrompt
                if matches!(self.prompt_response, WatchCausalChain::Pending) =>
            {
                self.prompt_response = WatchCausalChain::PredecessorSeen;
                Ok(WatchChainFact::Prompt)
            }
            tau_proto::AgentMessageKind::WatchResponse
                if matches!(self.prompt_response, WatchCausalChain::PredecessorSeen) =>
            {
                self.prompt_response = WatchCausalChain::Complete;
                Ok(WatchChainFact::Response)
            }
            _ => Err("watch prompt/response chain duplicated or inverted"),
        }
    }

    /// Returns whether the content chain admitted both facts.
    fn is_complete(self) -> bool {
        matches!(self.prompt_response, WatchCausalChain::Complete)
    }
}

/// Configured text for one closed content-chain watch action.
struct WatchChainContents {
    /// Expected direct user prompt.
    prompt: String,
    /// Expected final child response.
    response: String,
    /// Main-agent response after both facts.
    completion: String,
}

/// Narrow current watch action needed during live notification admission.
enum WatchExpectation {
    /// One exact next fact in an ordered S1 batch.
    Ordered(WatchNotificationV2),
    /// The S2 independent causal-chain grammar.
    Chains,
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
    /// Durable parent-to-child identities learned from successful starts.
    child_agents: Vec<ChildAgentCheckpoint>,
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

/// One immutable parent-to-child association learned from an `agent_start`
/// result.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ChildAgentCheckpoint {
    /// Parent agent that issued the exact start call.
    parent_agent_id: tau_proto::AgentId,
    /// Zero-based start-result ordinal within this parent's bound lane.
    start_ordinal: usize,
    /// Harness-minted child agent returned by the tool.
    child_agent_id: tau_proto::AgentId,
}

/// Validated state loaded from a checkpoint or initialized empty.
struct RestoredScenarioState {
    /// Validated next action index for each lane.
    cursors: Vec<usize>,
    /// Validated immutable live-agent lane associations.
    agent_lanes: HashMap<tau_proto::AgentId, usize>,
    /// Validated ordered parent-to-child associations.
    child_agents: HashMap<tau_proto::AgentId, Vec<tau_proto::AgentId>>,
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
                let supports_standalone_compaction =
                    cx.config.scenario.enables_standalone_compaction();
                cx.state.scenario = Some(cx.config.scenario);
                cx.state.lane_cursors = restored.cursors;
                cx.state.agent_lanes = restored.agent_lanes;
                cx.state.child_agents = restored.child_agents;
                cx.state.checkpoint = checkpoint;
                cx.state.trace = Some(Arc::new(Mutex::new(trace)));
                cx.handle
                    .emit_transient(Event::ProviderModelsDeclared(model_snapshot(
                        supports_standalone_compaction,
                    )))?;
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
                tau_proto::EventSelector::Exact(EventName::AGENT_MESSAGE_RECEIVED),
                |cx| {
                    let Event::AgentMessageReceived(message) = cx.delivery.event() else {
                        return Ok(());
                    };
                    cx.state.record_watch_notification(message)
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
            .on_raw_live(
                tau_proto::EventSelector::Exact(EventName::TOOL_ERROR),
                |cx| cx.state.record_dummy_repair_event(cx.delivery.event()),
            )
            .on_raw_live(
                tau_proto::EventSelector::Exact(EventName::PROVIDER_TOOL_ERROR),
                |cx| cx.state.record_dummy_repair_event(cx.delivery.event()),
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

fn model_snapshot(supports_standalone_compaction: bool) -> ProviderModelsDeclared {
    ProviderModelsDeclared {
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
            supports_standalone_compaction,
            standalone_compaction_threshold: None,
            cache_policy: None,
            est_uncached_input_cost_1m_usd: Default::default(),
            est_cached_input_cost_1m_usd: Default::default(),
            est_cache_write_input_cost_1m_usd: Default::default(),
            est_output_cost_1m_usd: Default::default(),
            est_cache_storage_cost_1m_token_hour_usd: None,
        }],
    }
}

impl FakeState {
    /// Returns the child owned by the parent's most recently consumed start.
    fn current_child_agent(
        &self,
        parent_agent_id: &tau_proto::AgentId,
    ) -> Option<&tau_proto::AgentId> {
        self.child_agents
            .get(parent_agent_id)
            .and_then(|children| children.last())
    }

    /// Validates and traces the exact live S6 durable/derived repair pair
    /// without turning either observation into provider work.
    fn record_dummy_repair_event(&mut self, event: &Event) -> ClientResult<()> {
        let (error, label) = match event {
            Event::ToolError(error) => (error, "repair_tool_error"),
            Event::ProviderToolError(error) => (error, "repair_provider_tool_error"),
            _ => return Ok(()),
        };
        let Some(ScenarioConfig::V2(scenario)) = self.scenario.as_ref() else {
            return Ok(());
        };
        let declared = scenario.lanes.iter().find_map(|lane| {
            lane.actions.iter().find_map(|action| match action {
                ScenarioActionV2::DummyToolRepair {
                    call_id,
                    diagnostic,
                    ..
                } if call_id == &error.call_id => Some((call_id, diagnostic.as_str())),
                _ => None,
            })
        });
        let current = scenario.lanes.iter().enumerate().find_map(|(index, lane)| {
            lane.actions
                .get(*self.lane_cursors.get(index)?)
                .and_then(|action| match action {
                    ScenarioActionV2::DummyToolRepair {
                        call_id,
                        diagnostic,
                        ..
                    } => Some((call_id, diagnostic.as_str())),
                    _ => None,
                })
        });
        let Some((current_call_id, current_diagnostic)) = current else {
            return if declared.is_some() {
                Err(ClientError::handler(
                    "dummy repair live event arrived outside its closed current action",
                ))
            } else {
                Ok(())
            };
        };
        if &error.call_id != current_call_id {
            return Err(ClientError::handler(
                "dummy repair live event targeted the wrong call",
            ));
        }
        if error.tool_name.as_str() != tau_ext_test_dummy::RESTART_TEST_DUMMY_TOOL_NAME
            || error.tool_type != ToolType::Function
            || error.message != current_diagnostic
        {
            return Err(ClientError::handler(
                "dummy repair live event did not match the closed call diagnostic",
            ));
        }
        match (&mut self.repair_progress, event) {
            (None, Event::ProviderToolError(_)) => {
                self.repair_progress = Some(DummyRepairProgress {
                    call_id: error.call_id.clone(),
                    phase: DummyRepairPhase::AwaitingToolError,
                });
            }
            (Some(progress), Event::ToolError(_))
                if progress.call_id == error.call_id
                    && matches!(progress.phase, DummyRepairPhase::AwaitingToolError) =>
            {
                progress.phase = DummyRepairPhase::Complete;
            }
            (Some(progress), _) if progress.call_id != error.call_id => {
                return Err(ClientError::handler(
                    "dummy repair live event targeted a second call",
                ));
            }
            _ => {
                return Err(ClientError::handler(
                    "dummy repair live events were duplicated or out of order",
                ));
            }
        }
        self.trace(&format!("call_id={} {label}", error.call_id))
    }

    fn record_watch_notification(&mut self, message: &AgentMessageReceived) -> ClientResult<()> {
        let model_visible = matches!(
            message.kind,
            tau_proto::AgentMessageKind::WatchResponse | tau_proto::AgentMessageKind::WatchPrompt
        );
        if !model_visible {
            return Ok(());
        }
        let Some(expected_child) = self.current_child_agent(&message.recipient_id) else {
            return Err(ClientError::handler(
                "unexpected model-visible watch recipient",
            ));
        };
        if expected_child != &message.sender_id {
            return Err(ClientError::handler(
                "unexpected model-visible watch sender",
            ));
        }
        let Some(lane_index) = self.agent_lanes.get(&message.recipient_id).copied() else {
            return Err(ClientError::handler(
                "watch recipient has no validated scenario lane",
            ));
        };
        let cursor = self.lane_cursors.get(lane_index).copied().unwrap_or(0);
        let progress = self
            .watch_progress
            .get(&message.recipient_id)
            .copied()
            .unwrap_or_default();
        let queued = self
            .watch_notifications
            .get(&message.recipient_id)
            .map(VecDeque::len)
            .unwrap_or_default();
        let next = progress + queued;
        let expectation = {
            let action = self
                .v2()?
                .lanes
                .get(lane_index)
                .and_then(|lane| lane.actions.get(cursor))
                .ok_or_else(|| {
                    ClientError::handler("watch delivery has no current closed action")
                })?;
            match action {
                ScenarioActionV2::WatchNotifications { notifications, .. } => {
                    WatchExpectation::Ordered(notifications.get(next).cloned().ok_or_else(
                        || {
                            ClientError::handler(
                                "watch delivery exceeded the ordered closed action",
                            )
                        },
                    )?)
                }
                ScenarioActionV2::WatchNotificationChains { .. } => WatchExpectation::Chains,
                _ => {
                    return Err(ClientError::handler(
                        "watch delivery has no current closed action",
                    ));
                }
            }
        };
        let (expected_notification, chain_progress) = match expectation {
            WatchExpectation::Ordered(expected) => (expected, None),
            WatchExpectation::Chains => {
                let mut progress = self
                    .watch_chain_progress
                    .get(&message.recipient_id)
                    .copied()
                    .unwrap_or_default();
                let fact = progress
                    .admit(message)
                    .map_err(|detail| self.mismatch(cursor, detail))?;
                let configured_content = match fact {
                    WatchChainFact::Prompt | WatchChainFact::Response => {
                        let action = &self.v2()?.lanes[lane_index].actions[cursor];
                        let ScenarioActionV2::WatchNotificationChains {
                            prompt, response, ..
                        } = action
                        else {
                            unreachable!("validated current action changed")
                        };
                        Some(match fact {
                            WatchChainFact::Prompt => prompt.clone(),
                            WatchChainFact::Response => response.clone(),
                        })
                    }
                };
                let expected = match fact {
                    WatchChainFact::Prompt => WatchNotificationV2::Prompt {
                        content: configured_content.expect("prompt content selected"),
                    },
                    WatchChainFact::Response => WatchNotificationV2::Response {
                        content: configured_content.expect("response content selected"),
                    },
                };
                (expected, Some(progress))
            }
        };
        self.validate_watch_notification_payload(cursor, message, &expected_notification)?;
        if let Some(progress) = chain_progress {
            self.watch_chain_progress
                .insert(message.recipient_id.clone(), progress);
        }
        self.watch_notifications
            .entry(message.recipient_id.clone())
            .or_default()
            .push_back(message.clone());
        Ok(())
    }

    fn validate_watch_notification_payload(
        &mut self,
        cursor: usize,
        message: &AgentMessageReceived,
        expected: &WatchNotificationV2,
    ) -> ClientResult<()> {
        match expected {
            WatchNotificationV2::Response { content }
                if message.kind == tau_proto::AgentMessageKind::WatchResponse
                    && message.watch_provider_status.is_none()
                    && &message.message == content =>
            {
                Ok(())
            }
            WatchNotificationV2::Prompt { content }
                if message.kind == tau_proto::AgentMessageKind::WatchPrompt
                    && message.watch_provider_status.is_none()
                    && &message.message == content =>
            {
                Ok(())
            }
            _ => Err(self.mismatch(cursor, "watch notification typed payload mismatch")),
        }
    }

    fn handle_watch_batch_prompt(
        &mut self,
        lane_index: usize,
        cursor: usize,
        prompt: &tau_proto::AgentPromptCreated,
        expected: Vec<WatchNotificationV2>,
        response: String,
        handle: &tau_client::ClientHandle,
    ) -> ClientResult<()> {
        let progress = self
            .watch_progress
            .get(&prompt.agent_id)
            .copied()
            .unwrap_or_default();
        let queued_len = self
            .watch_notifications
            .get(&prompt.agent_id)
            .map(VecDeque::len)
            .unwrap_or_default();
        let Some(next_progress) = progress.checked_add(queued_len) else {
            return Err(self.mismatch(cursor, "watch batch progress overflow"));
        };
        if queued_len == 0 || expected.len() < next_progress {
            return Err(self.mismatch(cursor, "watch batch stage is empty or over-consumed"));
        }
        self.validate_watch_notification_messages(
            cursor,
            prompt,
            &expected[progress..next_progress],
        )?;
        self.agent_lanes
            .entry(prompt.agent_id.clone())
            .or_insert(lane_index);
        let queued = self
            .watch_notifications
            .get_mut(&prompt.agent_id)
            .expect("validated watch queue exists");
        queued.drain(..queued_len);
        if queued.is_empty() {
            self.watch_notifications.remove(&prompt.agent_id);
        }
        handle.emit_transient(Event::ProviderPromptSubmittedReported(
            ProviderPromptSubmitted {
                agent_prompt_id: prompt.agent_prompt_id.clone(),
                originator: prompt.originator.clone(),
            },
        ))?;
        let terminal_response = if next_progress == expected.len() {
            self.watch_progress.remove(&prompt.agent_id);
            self.lane_cursors[lane_index] += 1;
            self.persist_cursors()?;
            response
        } else {
            self.watch_progress
                .insert(prompt.agent_id.clone(), next_progress);
            "watch notification accepted".to_owned()
        };
        handle.emit_transient(Event::ProviderResponseFinishedReported(finished(
            prompt,
            vec![assistant_message(terminal_response)],
            ProviderStopReason::EndTurn,
        )))
    }

    fn handle_watch_chain_prompt(
        &mut self,
        lane_index: usize,
        cursor: usize,
        prompt: &tau_proto::AgentPromptCreated,
        contents: WatchChainContents,
        handle: &tau_client::ClientHandle,
    ) -> ClientResult<()> {
        let progress = self
            .watch_progress
            .get(&prompt.agent_id)
            .copied()
            .unwrap_or_default();
        let actual = self
            .watch_notifications
            .get(&prompt.agent_id)
            .cloned()
            .ok_or_else(|| self.mismatch(cursor, "watch chain stage is empty"))?;
        let Some(next_progress) = progress.checked_add(actual.len()) else {
            return Err(self.mismatch(cursor, "watch chain progress overflow"));
        };
        if actual.is_empty() || 2 < next_progress {
            return Err(self.mismatch(cursor, "watch chain is over-consumed"));
        }
        let expected = actual
            .iter()
            .map(|message| match message.kind {
                tau_proto::AgentMessageKind::WatchPrompt => Ok(WatchNotificationV2::Prompt {
                    content: contents.prompt.clone(),
                }),
                tau_proto::AgentMessageKind::WatchResponse => Ok(WatchNotificationV2::Response {
                    content: contents.response.clone(),
                }),
                _ => Err(self.mismatch(cursor, "watch chain contains an invalid kind")),
            })
            .collect::<ClientResult<Vec<_>>>()?;
        self.validate_watch_notification_messages(cursor, prompt, &expected)?;
        self.agent_lanes
            .entry(prompt.agent_id.clone())
            .or_insert(lane_index);
        let queued = self
            .watch_notifications
            .get_mut(&prompt.agent_id)
            .expect("validated watch queue exists");
        queued.drain(..actual.len());
        if queued.is_empty() {
            self.watch_notifications.remove(&prompt.agent_id);
        }
        handle.emit_transient(Event::ProviderPromptSubmittedReported(
            ProviderPromptSubmitted {
                agent_prompt_id: prompt.agent_prompt_id.clone(),
                originator: prompt.originator.clone(),
            },
        ))?;
        let terminal_response = if next_progress == 2 {
            let admitted = self
                .watch_chain_progress
                .remove(&prompt.agent_id)
                .unwrap_or_default();
            if !admitted.is_complete() {
                return Err(self.mismatch(cursor, "watch chain completed without both facts"));
            }
            self.watch_progress.remove(&prompt.agent_id);
            self.lane_cursors[lane_index] += 1;
            self.persist_cursors()?;
            contents.completion
        } else {
            self.watch_progress
                .insert(prompt.agent_id.clone(), next_progress);
            "watch notification accepted".to_owned()
        };
        handle.emit_transient(Event::ProviderResponseFinishedReported(finished(
            prompt,
            vec![assistant_message(terminal_response)],
            ProviderStopReason::EndTurn,
        )))
    }

    fn validate_watch_notification_messages(
        &mut self,
        cursor: usize,
        prompt: &tau_proto::AgentPromptCreated,
        expected: &[WatchNotificationV2],
    ) -> ClientResult<()> {
        let Some(child_agent_id) = self.current_child_agent(&prompt.agent_id).cloned() else {
            return Err(self.mismatch(cursor, "watch prompt has no validated child identity"));
        };
        let actual = self
            .watch_notifications
            .get(&prompt.agent_id)
            .map(|messages| {
                messages
                    .iter()
                    .take(expected.len())
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if actual.len() != expected.len() {
            self.trace(&format!(
                "watch batch expected={} actual={:?}",
                expected.len(),
                actual
                    .iter()
                    .map(|message| message.kind)
                    .collect::<Vec<_>>()
            ))?;
            return Err(self.mismatch(cursor, "watch notification batch length mismatch"));
        }
        let mut expected_user_text = Vec::new();
        for (message, expected) in actual.iter().zip(expected) {
            if message.sender_id != child_agent_id || message.recipient_id != prompt.agent_id {
                return Err(self.mismatch(cursor, "watch notification agent identity mismatch"));
            }
            let text = match expected {
                WatchNotificationV2::Response { content } => {
                    let body = tau_proto::escape_exact_sentinel_close(
                        content,
                        "</response>",
                        "&lt;/response&gt;",
                    );
                    format!(
                        "[tau-internal]: Watched agent {child_agent_id} emitted a response\n\n\
                         <response>\n{body}\n</response>"
                    )
                }
                WatchNotificationV2::Prompt { content } => {
                    let body = tau_proto::escape_exact_sentinel_close(
                        content,
                        "</prompt>",
                        "&lt;/prompt&gt;",
                    );
                    format!(
                        "[tau-internal]: Watched agent {child_agent_id} received a user prompt\n\n\
                         <prompt>\n{body}\n</prompt>"
                    )
                }
            };
            expected_user_text.push(text);
        }
        let actual_user_text = scenario_user_texts(prompt);
        if !actual_user_text.ends_with(&expected_user_text) {
            self.trace(&format!(
                "watch markup expected={expected_user_text:?} actual={actual_user_text:?}"
            ))?;
            return Err(self.mismatch(cursor, "watch notification prompt markup mismatch"));
        }
        Ok(())
    }

    fn trace_watch_action_stage(
        &mut self,
        lane_id: &str,
        cursor: usize,
        action_count: usize,
        lane_index: usize,
        prompt: &tau_proto::AgentPromptCreated,
        before: usize,
    ) -> ClientResult<()> {
        if self.lane_cursors[lane_index] != before {
            self.trace(&format!(
                "lane={lane_id} action={cursor} matched remaining={}",
                action_count - self.lane_cursors[lane_index]
            ))
        } else {
            self.trace(&format!(
                "lane={lane_id} action={cursor} staged watch_progress={}",
                self.watch_progress
                    .get(&prompt.agent_id)
                    .copied()
                    .unwrap_or_default()
            ))
        }
    }

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
        handle.emit_transient(Event::ProviderPromptSubmittedReported(
            ProviderPromptSubmitted {
                agent_prompt_id: prompt.agent_prompt_id.clone(),
                originator: prompt.originator.clone(),
            },
        ))?;
        let terminal = match turn {
            ScenarioTurnV1::Text {
                user_text,
                deltas,
                response,
            } => {
                self.require_human_ui_user_text(index, prompt, &user_text)?;
                for text in deltas {
                    handle.emit_transient(Event::ProviderResponseUpdatedReported(
                        ProviderResponseUpdated {
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
                        },
                    ))?;
                }
                Event::ProviderResponseFinishedReported(finished(
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
                self.require_human_ui_user_text(index, prompt, &user_text)?;
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
                Event::ProviderResponseFinishedReported(finished(
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
                Event::ProviderResponseFinishedReported(finished(
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
        handle.emit_transient(terminal)
    }

    fn handle_v2_prompt(
        &mut self,
        prompt: &tau_proto::AgentPromptCreated,
        handle: &tau_client::ClientHandle,
    ) -> ClientResult<()> {
        if prompt.model.provider.as_str() != "fake" || prompt.model.model.as_str() != "test" {
            return Err(self.mismatch(0, "model/operation mismatch"));
        }
        let lane_index = self.select_v2_lane(prompt)?;
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
        if !action.matches_operation(prompt.operation) {
            return Err(self.mismatch(cursor, "prompt operation mismatch"));
        }
        let action = match action {
            ScenarioActionV2::WatchNotifications {
                notifications,
                response,
            } => {
                let before = self.lane_cursors[lane_index];
                self.trace(&format!(
                    "lane={lane_id} action={cursor} prompt_id={}",
                    prompt.agent_prompt_id
                ))?;
                self.handle_watch_batch_prompt(
                    lane_index,
                    cursor,
                    prompt,
                    notifications,
                    response,
                    handle,
                )?;
                self.trace_watch_action_stage(
                    &lane_id,
                    cursor,
                    action_count,
                    lane_index,
                    prompt,
                    before,
                )?;
                return Ok(());
            }
            ScenarioActionV2::WatchNotificationChains {
                prompt: prompt_content,
                response,
                completion,
            } => {
                let before = self.lane_cursors[lane_index];
                self.trace(&format!(
                    "lane={lane_id} action={cursor} prompt_id={}",
                    prompt.agent_prompt_id
                ))?;
                self.handle_watch_chain_prompt(
                    lane_index,
                    cursor,
                    prompt,
                    WatchChainContents {
                        prompt: prompt_content,
                        response,
                        completion,
                    },
                    handle,
                )?;
                self.trace_watch_action_stage(
                    &lane_id,
                    cursor,
                    action_count,
                    lane_index,
                    prompt,
                    before,
                )?;
                return Ok(());
            }
            action => action,
        };
        self.validate_and_commit_v2_action(lane_index, cursor, prompt, &action)?;
        self.trace(&format!(
            "lane={lane_id} action={cursor} prompt_id={}",
            prompt.agent_prompt_id
        ))?;
        handle.emit_transient(Event::ProviderPromptSubmittedReported(
            ProviderPromptSubmitted {
                agent_prompt_id: prompt.agent_prompt_id.clone(),
                originator: prompt.originator.clone(),
            },
        ))?;
        self.trace(&format!(
            "lane={lane_id} action={cursor} matched remaining={}",
            action_count - self.lane_cursors[lane_index]
        ))?;

        self.emit_v2_action(prompt, handle, action)
    }

    fn select_v2_lane(&self, prompt: &tau_proto::AgentPromptCreated) -> ClientResult<usize> {
        if let Some(lane) = self.agent_lanes.get(&prompt.agent_id) {
            return Ok(*lane);
        }
        let lane_index = if let Some(ctx_id) = prompt.ctx_id.as_deref() {
            let scenario = self.v2()?;
            scenario
                .lanes
                .iter()
                .position(|lane| lane.ctx_id == ctx_id)
                .or_else(|| {
                    (PUBLIC_PTY_DYNAMIC_LANE_SCENARIOS.contains(&scenario.name.as_str())
                        && scenario.lanes.len() == 1
                        && ctx_id.starts_with("ui-prompt-"))
                    .then_some(0)
                })
                .ok_or_else(|| {
                    ClientError::handler("scenario first mismatch: unknown lane ctx_id")
                })?
        } else if self.v2()?.lanes.len() == 1 {
            0
        } else {
            if !self
                .child_agents
                .values()
                .flatten()
                .any(|child_agent_id| child_agent_id == &prompt.agent_id)
            {
                return Err(ClientError::handler(
                    "scenario first mismatch: no-context agent is not the validated child",
                ));
            }
            let actual = latest_provider_user_text(prompt);
            let candidates = self
                .v2()?
                .lanes
                .iter()
                .enumerate()
                .filter(|(index, lane)| {
                    self.lane_cursors.get(*index) == Some(&0)
                        && !self.agent_lanes.values().any(|bound| bound == index)
                        && lane
                            .actions
                            .first()
                            .and_then(ScenarioActionV2::binding_user_text)
                            .is_some_and(|expected| {
                                actual.as_deref().is_some_and(|actual| {
                                    fixture_user_text_matches(actual, expected)
                                })
                            })
                })
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            match candidates.as_slice() {
                [index] => *index,
                [] => {
                    return Err(ClientError::handler(
                        "scenario first mismatch: initial prompt matched no unbound lane",
                    ));
                }
                _ => {
                    return Err(ClientError::handler(
                        "scenario first mismatch: initial prompt matched ambiguous lanes",
                    ));
                }
            }
        };
        if self
            .agent_lanes
            .values()
            .any(|bound_lane| *bound_lane == lane_index)
        {
            return Err(ClientError::handler(
                "scenario first mismatch: lane is already bound to another agent",
            ));
        }
        Ok(lane_index)
    }

    fn emit_v2_action(
        &mut self,
        prompt: &tau_proto::AgentPromptCreated,
        handle: &tau_client::ClientHandle,
        action: ScenarioActionV2,
    ) -> ClientResult<()> {
        match action {
            ScenarioActionV2::Text { response, .. }
            | ScenarioActionV2::CompactedText { response, .. }
            | ScenarioActionV2::DummyToolResult { response, .. }
            | ScenarioActionV2::DummyToolRepair { response, .. }
            | ScenarioActionV2::AgentStartResult { response, .. }
            | ScenarioActionV2::AgentWatchResult { response, .. }
            | ScenarioActionV2::WatchNotifications { response, .. }
            | ScenarioActionV2::WatchNotificationChains {
                completion: response,
                ..
            }
            | ScenarioActionV2::CoreShellCreateResult { response, .. }
            | ScenarioActionV2::CoreShellResumeEditResult { response, .. } => {
                emit_text_response(prompt, handle, response)
            }
            ScenarioActionV2::StandaloneCompaction { summary } => {
                emit_text_response(prompt, handle, summary)
            }
            ScenarioActionV2::StandaloneCompactionError {
                failure_kind,
                error,
            }
            | ScenarioActionV2::Error {
                failure_kind,
                error,
                ..
            } => emit_error_response(prompt, handle, failure_kind, error),
            ScenarioActionV2::StandaloneCompactionHold { timeout_ms } => {
                self.emit_hold_until_cancel(prompt, handle, timeout_ms)
            }
            ScenarioActionV2::DummyToolCall { call_id, .. } => {
                emit_dummy_tool_call(prompt, handle, call_id)
            }
            action @ (ScenarioActionV2::AgentStartCall { .. }
            | ScenarioActionV2::AgentWatchCall { .. }) => {
                self.emit_agent_tool_call(prompt, handle, action)
            }
            ScenarioActionV2::CoreShellWorkdirCall { call_id, .. } => emit_tool_call(
                handle,
                prompt,
                call_id,
                "workdir",
                cbor_map(vec![("path", CborValue::Text("project".to_owned()))]),
            ),
            ScenarioActionV2::CoreShellWorkdirResult {
                edit_call_id,
                nonce,
                ..
            } => emit_tool_call(
                handle,
                prompt,
                edit_call_id,
                "edit",
                edit_arguments(1, 1, format!("before:{nonce}\n"), ""),
            ),
            ScenarioActionV2::CoreShellResumeEditCall { call_id, nonce, .. } => emit_tool_call(
                handle,
                prompt,
                call_id,
                "edit",
                edit_arguments(
                    1,
                    2,
                    format!("before:{nonce}\nafter:{nonce}\n"),
                    &format!("before:{nonce}"),
                ),
            ),
            ScenarioActionV2::Disconnect { reason, .. } => {
                self.trace(&format!("deliberate_disconnect={reason}"))?;
                Err(ClientError::handler(format!(
                    "deliberate scenario disconnect: {reason}"
                )))
            }
            ScenarioActionV2::HoldUntilCancel { timeout_ms, .. } => {
                self.emit_hold_until_cancel(prompt, handle, timeout_ms)
            }
            ScenarioActionV2::BarrierText {
                barrier,
                participants,
                response,
                ..
            } => self.emit_barrier_text(prompt, handle, barrier, participants, response),
        }
    }

    /// Queues one participant and completes its barrier when every lane
    /// arrived.
    fn emit_barrier_text(
        &mut self,
        prompt: &tau_proto::AgentPromptCreated,
        handle: &tau_client::ClientHandle,
        barrier: String,
        participants: usize,
        response: String,
    ) -> ClientResult<()> {
        let pending = self.barriers.entry(barrier.clone()).or_default();
        pending.push(BarrierParticipant {
            prompt: prompt.clone(),
            response,
        });
        if participants < pending.len() {
            return Err(ClientError::handler("barrier over-subscribed"));
        }
        if pending.len() == participants {
            let completed = self.barriers.remove(&barrier).unwrap_or_default();
            for participant in completed {
                emit_text_response(&participant.prompt, handle, participant.response)?;
            }
        }
        Ok(())
    }

    /// Starts one bounded cancellation hold and publishes its semantic
    /// readiness.
    fn emit_hold_until_cancel(
        &mut self,
        prompt: &tau_proto::AgentPromptCreated,
        handle: &tau_client::ClientHandle,
        timeout_ms: u64,
    ) -> ClientResult<()> {
        let (cancel, wait) = mpsc::channel();
        let (ready, readiness) = mpsc::sync_channel(0);
        let (completion, completed) = mpsc::channel();
        let trace = self
            .trace
            .clone()
            .ok_or_else(|| ClientError::handler("trace not configured"))?;
        let worker_handle = handle.clone();
        let held_prompt = prompt.clone();
        let prompt_id = prompt.agent_prompt_id.clone();
        let worker_prompt_id = prompt_id.clone();
        let join = thread::spawn(move || {
            if ready.send(()).is_err() {
                return HoldOutcome::Shutdown;
            }
            match wait.recv_timeout(Duration::from_millis(timeout_ms)) {
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let mut terminal =
                        finished(&held_prompt, Vec::new(), ProviderStopReason::Error);
                    terminal.error = Some("deterministic hold timed out".to_owned());
                    terminal.failure_kind = Some(tau_proto::ProviderFailureKind::Unknown);
                    let _ = write_shared_trace(
                        &trace,
                        &format!("prompt_id={worker_prompt_id} hold_timeout"),
                    );
                    let _ = completion.send(());
                    let _ = worker_handle
                        .emit_transient(Event::ProviderResponseFinishedReported(terminal));
                    HoldOutcome::TimedOut
                }
                Ok(canceled_by) => {
                    let _ = write_shared_trace(
                        &trace,
                        &format!(
                            "prompt_id={worker_prompt_id} hold_canceled \
                             canceled_by={canceled_by}"
                        ),
                    );
                    let mut terminal =
                        finished(&held_prompt, Vec::new(), ProviderStopReason::Error);
                    terminal.error = Some("(cancelled by harness)".to_owned());
                    terminal.failure_kind = Some(tau_proto::ProviderFailureKind::Unknown);
                    let _ = completion.send(());
                    let _ = worker_handle
                        .emit_transient(Event::ProviderResponseFinishedReported(terminal));
                    HoldOutcome::Canceled(canceled_by)
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => HoldOutcome::Shutdown,
            }
        });
        if let Err(error) = readiness.recv_timeout(Duration::from_secs(1)) {
            drop(cancel);
            let _ = join.join();
            return Err(ClientError::handler(format!(
                "hold readiness failed: {error}"
            )));
        }
        self.holds.insert(
            prompt.agent_prompt_id.clone(),
            PendingHold {
                cancel,
                join,
                completed,
            },
        );
        self.trace(&format!("prompt_id={prompt_id} hold_ready"))?;
        handle.request_notice(
            format!(
                "e2e_fake_provider.hold_ready {}",
                serde_json::to_string(&serde_json::json!({
                    "prompt_id": prompt_id.to_string(),
                }))
                .map_err(|error| ClientError::handler(error.to_string()))?
            ),
            tau_proto::NoticeLevel::Info,
        )
    }

    fn emit_agent_tool_call(
        &mut self,
        prompt: &tau_proto::AgentPromptCreated,
        handle: &tau_client::ClientHandle,
        action: ScenarioActionV2,
    ) -> ClientResult<()> {
        match action {
            ScenarioActionV2::AgentStartCall {
                call_id,
                prompt: child_prompt,
                role,
                ..
            } => {
                let fields = vec![
                    ("prompt", CborValue::Text(child_prompt)),
                    ("role", CborValue::Text(role)),
                ];
                emit_tool_call(handle, prompt, call_id, "agent_start", cbor_map(fields))
            }
            ScenarioActionV2::AgentWatchCall { call_id, .. } => {
                let Some(child_agent_id) = self.current_child_agent(&prompt.agent_id) else {
                    return Err(self.mismatch(
                        0,
                        "agent_watch call has no validated agent_start child identity",
                    ));
                };
                let child_agent_id = child_agent_id.to_string();
                emit_tool_call(
                    handle,
                    prompt,
                    call_id,
                    "agent_watch",
                    cbor_map(vec![
                        ("agent_id", CborValue::Text(child_agent_id)),
                        ("enable", CborValue::Bool(true)),
                    ]),
                )
            }
            _ => unreachable!("agent tool-call helper received another action"),
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
        handle.request_notice(
            format!(
                "e2e_fake_provider.cancel_completed {}",
                serde_json::to_string(&serde_json::json!({
                    "selected": prompt_id.to_string(),
                    "canceled_by": canceled_by.to_string(),
                    "active_before": active,
                }))
                .map_err(|error| ClientError::handler(error.to_string()))?
            ),
            tau_proto::NoticeLevel::Trace,
        )?;
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

    fn validate_v2_action(
        &mut self,
        cursor: usize,
        prompt: &tau_proto::AgentPromptCreated,
        action: &ScenarioActionV2,
    ) -> ClientResult<()> {
        match action {
            ScenarioActionV2::DummyToolCall { .. } => {
                let tool_name = tau_ext_test_dummy::RESTART_TEST_DUMMY_TOOL_NAME;
                let expected_schema = serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false,
                });
                if prompt.tools.len() != 1
                    || prompt.tools[0].name.as_str() != tool_name
                    || prompt.tools[0].tool_type != ToolType::Function
                    || prompt.tools[0].parameters.as_ref() != Some(&expected_schema)
                {
                    return Err(self.mismatch(cursor, "dummy tool snapshot mismatch"));
                }
            }
            ScenarioActionV2::DummyToolResult { call_id, .. } => {
                let results = prompt
                    .context
                    .flatten_iter()
                    .filter_map(|item| match item {
                        ContextItem::ToolResult(result) => Some(result),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                if results.len() != 1
                    || results[0].call_id != *call_id
                    || results[0].tool_type != ToolType::Function
                    || results[0].status != tau_proto::ToolResultStatus::Success
                    || results[0].output.body != "restart succeeded"
                {
                    return Err(self.mismatch(cursor, "dummy tool result continuity mismatch"));
                }
            }
            ScenarioActionV2::DummyToolRepair {
                call_id,
                diagnostic,
                ..
            } => {
                require_repaired_dummy_result(prompt, call_id, diagnostic)
                    .map_err(|detail| self.mismatch(cursor, detail))?;
            }
            ScenarioActionV2::StandaloneCompaction { summary: _ }
            | ScenarioActionV2::StandaloneCompactionError {
                failure_kind: _,
                error: _,
            }
            | ScenarioActionV2::StandaloneCompactionHold { timeout_ms: _ } => {
                // The Chat Completions adapter owns static no-tools lowering.
                // This provider seam instead proves the harness supplied the
                // standalone operation and nonempty compactable transcript.
                if prompt.context.blocks.is_empty() {
                    return Err(self.mismatch(
                        cursor,
                        "standalone compaction request lacks transcript context",
                    ));
                }
            }
            ScenarioActionV2::CompactedText {
                user_text: _,
                summary,
                removed_user_text,
                response: _,
            } => {
                let context = serde_json::to_string(&prompt.context)
                    .map_err(|error| ClientError::handler(error.to_string()))?;
                if !context.contains(&summary.replace('\n', "\\n"))
                    || context.contains(removed_user_text)
                {
                    return Err(self.mismatch(
                        cursor,
                        "replacement window did not replace prior transcript",
                    ));
                }
            }
            ScenarioActionV2::AgentStartCall { .. } => {
                let expects_agent_watch = self.v2()?.lanes.iter().any(|lane| {
                    lane.actions
                        .iter()
                        .any(|action| matches!(action, ScenarioActionV2::AgentWatchCall { .. }))
                });
                let expected_schema = agent_start_parameters();
                let start = prompt
                    .tools
                    .iter()
                    .find(|tool| tool.name.as_str() == "agent_start");
                let watch = prompt
                    .tools
                    .iter()
                    .find(|tool| tool.name.as_str() == "agent_watch");
                if prompt.tools.len() != usize::from(expects_agent_watch) + 1
                    || start.is_none_or(|tool| {
                        tool.tool_type != ToolType::Function
                            || tool.parameters.as_ref() != Some(&expected_schema)
                    })
                    || expects_agent_watch
                        != watch.is_some_and(|tool| {
                            tool.tool_type == ToolType::Function
                                && tool.parameters.as_ref() == Some(&agent_watch_parameters())
                        })
                {
                    self.trace(&format!(
                        "agent_start tools={}",
                        serde_json::to_string(&prompt.tools)
                            .unwrap_or_else(|_| "<unserializable>".to_owned())
                    ))?;
                    return Err(self.mismatch(cursor, "agent_start tool snapshot mismatch"));
                }
            }
            ScenarioActionV2::AgentStartResult { call_id, .. } => {
                let results = prompt
                    .context
                    .blocks
                    .iter()
                    .rev()
                    .find_map(|block| match block {
                        tau_proto::ContextBlock::ToolResults(results) => Some(&results.items),
                        _ => None,
                    })
                    .ok_or_else(|| {
                        self.mismatch(cursor, "agent_start result lacks a tool-results block")
                    })?;
                if results.len() != 1
                    || results[0].call_id != *call_id
                    || results[0].tool_type != ToolType::Function
                    || results[0].status != tau_proto::ToolResultStatus::Success
                {
                    return Err(self.mismatch(cursor, "agent_start result continuity mismatch"));
                }
                let Some(self_agent_id) =
                    cbor_map_text_field(&results[0].output.raw, "self_agent_id")
                else {
                    return Err(self.mismatch(cursor, "agent_start result lacks self_agent_id"));
                };
                let Some(child_agent_id) =
                    cbor_map_text_field(&results[0].output.raw, "sub_agent_id")
                else {
                    return Err(self.mismatch(cursor, "agent_start result lacks sub_agent_id"));
                };
                let self_agent_id = tau_proto::AgentId::parse(self_agent_id)
                    .map_err(|_| self.mismatch(cursor, "agent_start returned invalid self id"))?;
                let child_agent_id = tau_proto::AgentId::parse(child_agent_id)
                    .map_err(|_| self.mismatch(cursor, "agent_start returned invalid child id"))?;
                let CborValue::Map(entries) = &results[0].output.raw else {
                    return Err(self.mismatch(cursor, "agent_start result is not a map"));
                };
                if entries.len() != 2
                    || self_agent_id != prompt.agent_id
                    || child_agent_id == self_agent_id
                {
                    return Err(self.mismatch(cursor, "agent_start result identity mismatch"));
                }
                let children = self
                    .child_agents
                    .get(&self_agent_id)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                if children.contains(&child_agent_id) {
                    return Err(self.mismatch(cursor, "agent_start child identity reused"));
                }
                if children.len() >= MAX_AGENT_START_PAIRS {
                    return Err(self.mismatch(cursor, "agent_start child count exceeded"));
                }
                self.child_agents
                    .entry(self_agent_id)
                    .or_default()
                    .push(child_agent_id);
            }
            ScenarioActionV2::AgentWatchCall { .. } => {
                let watch = prompt
                    .tools
                    .iter()
                    .find(|tool| tool.name.as_str() == "agent_watch");
                let start = prompt
                    .tools
                    .iter()
                    .find(|tool| tool.name.as_str() == "agent_start");
                if self
                    .child_agents
                    .get(&prompt.agent_id)
                    .is_none_or(|children| children.len() != 1)
                    || prompt.tools.len() != 2
                    || watch.is_none_or(|tool| {
                        tool.tool_type != ToolType::Function
                            || tool.parameters.as_ref() != Some(&agent_watch_parameters())
                    })
                    || start.is_none_or(|tool| {
                        tool.tool_type != ToolType::Function
                            || tool.parameters.as_ref() != Some(&agent_start_parameters())
                    })
                {
                    return Err(self.mismatch(cursor, "agent_watch tool snapshot mismatch"));
                }
            }
            ScenarioActionV2::AgentWatchResult {
                call_id,
                expectation,
                ..
            } => {
                let Some(child_agent_id) = self.current_child_agent(&prompt.agent_id) else {
                    return Err(self.mismatch(
                        cursor,
                        "agent_watch result has no validated agent_start child identity",
                    ));
                };
                let expected = match expectation {
                    AgentWatchResultExpectationV2::Enabled => {
                        format!("Watching agent `{child_agent_id}`")
                    }
                    AgentWatchResultExpectationV2::DispatchUncertainUnknown => {
                        format!(
                            "Watching agent `{child_agent_id}`; current status: \
                             dispatch uncertain (unknown)"
                        )
                    }
                };
                let latest_results = prompt
                    .context
                    .blocks
                    .iter()
                    .rev()
                    .find_map(|block| match block {
                        tau_proto::ContextBlock::ToolResults(results) => Some(&results.items),
                        _ => None,
                    })
                    .ok_or_else(|| {
                        self.mismatch(cursor, "agent_watch result lacks a tool-results block")
                    })?;
                if latest_results.len() != 1
                    || latest_results[0].call_id != *call_id
                    || latest_results[0].tool_type != ToolType::Function
                    || latest_results[0].status != tau_proto::ToolResultStatus::Success
                    || !matches!(
                        &latest_results[0].output.raw,
                        CborValue::Text(text) if text == &expected
                    )
                    || latest_results[0].output.body != expected
                {
                    return Err(self.mismatch(
                        cursor,
                        "agent_watch result continuity or sanitized text mismatch",
                    ));
                }
            }
            ScenarioActionV2::WatchNotifications { .. }
            | ScenarioActionV2::WatchNotificationChains { .. } => {
                return Err(self.mismatch(cursor, "watch batch bypassed bounded release"));
            }
            ScenarioActionV2::CoreShellWorkdirCall { .. }
            | ScenarioActionV2::CoreShellResumeEditCall { .. } => {
                let mut names = prompt
                    .tools
                    .iter()
                    .map(|tool| tool.name.as_str())
                    .collect::<Vec<_>>();
                names.sort_unstable();
                if names != ["edit", "workdir"] {
                    return Err(self.mismatch(cursor, "core-shell tool snapshot mismatch"));
                }
                if let ScenarioActionV2::CoreShellResumeEditCall { nonce, .. } = action {
                    let context = serde_json::to_string(&prompt.context)
                        .map_err(|error| ClientError::handler(error.to_string()))?;
                    if !context.contains(&format!("before:{nonce}")) {
                        return Err(
                            self.mismatch(cursor, "resumed provider context lacks old sentinel")
                        );
                    }
                    let workdirs = prompt
                        .system_prompt
                        .lines()
                        .filter(|line| line.starts_with("- default shell tools (`workdir`):"))
                        .collect::<Vec<_>>();
                    if workdirs.len() != 1 || !workdirs[0].contains("/shell-base/project") {
                        return Err(self.mismatch(
                            cursor,
                            "resumed provider context lacks restored core-shell workdir",
                        ));
                    }
                }
            }
            ScenarioActionV2::CoreShellWorkdirResult { call_id, .. }
            | ScenarioActionV2::CoreShellCreateResult { call_id, .. }
            | ScenarioActionV2::CoreShellResumeEditResult { call_id, .. } => {
                let results = prompt
                    .context
                    .flatten_iter()
                    .filter_map(|item| match item {
                        ContextItem::ToolResult(result) if result.call_id == *call_id => {
                            Some(result)
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                if results.len() != 1 || results[0].status != tau_proto::ToolResultStatus::Success {
                    return Err(self.mismatch(cursor, "core-shell result continuity mismatch"));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn validate_and_commit_v2_action(
        &mut self,
        lane_index: usize,
        cursor: usize,
        prompt: &tau_proto::AgentPromptCreated,
        action: &ScenarioActionV2,
    ) -> ClientResult<()> {
        if let Some(user_text) = action.binding_user_text() {
            self.require_v2_user_text(prompt, user_text)?;
        }
        self.validate_v2_action(cursor, prompt, action)?;
        self.agent_lanes
            .entry(prompt.agent_id.clone())
            .or_insert(lane_index);
        self.lane_cursors[lane_index] += 1;
        self.persist_cursors()
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
            child_agents: self
                .child_agents
                .iter()
                .flat_map(|(parent_agent_id, child_agent_ids)| {
                    child_agent_ids
                        .iter()
                        .enumerate()
                        .map(|(start_ordinal, child_agent_id)| ChildAgentCheckpoint {
                            parent_agent_id: parent_agent_id.clone(),
                            start_ordinal,
                            child_agent_id: child_agent_id.clone(),
                        })
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
        let actual = latest_provider_user_text(prompt);
        if !actual
            .as_deref()
            .is_some_and(|actual| fixture_user_text_matches(actual, expected))
        {
            self.trace(&format!(
                "last user expected={expected:?} actual={actual:?}"
            ))?;
            return Err(self.mismatch(index, "last user text mismatch"));
        }
        Ok(())
    }

    fn require_human_ui_user_text(
        &mut self,
        index: usize,
        prompt: &tau_proto::AgentPromptCreated,
        expected: &str,
    ) -> ClientResult<()> {
        let actual = latest_provider_user_text(prompt);
        let projected = project_fixture_human_ui_user_prompt(expected);
        if actual.as_deref() != Some(projected.as_str()) {
            self.trace(&format!(
                "last HumanUi user expected={expected:?} actual={actual:?}"
            ))?;
            return Err(self.mismatch(index, "last HumanUi user envelope mismatch"));
        }
        Ok(())
    }
}

/// Publishes one normal assistant-text provider completion.
fn emit_text_response(
    prompt: &tau_proto::AgentPromptCreated,
    handle: &tau_client::ClientHandle,
    response: String,
) -> ClientResult<()> {
    handle.emit_transient(Event::ProviderResponseFinishedReported(finished(
        prompt,
        vec![assistant_message(response)],
        ProviderStopReason::EndTurn,
    )))
}

/// Publishes one typed terminal provider failure.
fn emit_error_response(
    prompt: &tau_proto::AgentPromptCreated,
    handle: &tau_client::ClientHandle,
    failure_kind: tau_proto::ProviderFailureKind,
    error: String,
) -> ClientResult<()> {
    let mut terminal = finished(prompt, Vec::new(), ProviderStopReason::Error);
    terminal.error = Some(error);
    terminal.failure_kind = Some(failure_kind);
    handle.emit_transient(Event::ProviderResponseFinishedReported(terminal))
}

/// Publishes the allowlisted deterministic dummy-tool invocation.
fn emit_dummy_tool_call(
    prompt: &tau_proto::AgentPromptCreated,
    handle: &tau_client::ClientHandle,
    call_id: tau_proto::ToolCallId,
) -> ClientResult<()> {
    let tool_name = ToolName::new(tau_ext_test_dummy::RESTART_TEST_DUMMY_TOOL_NAME);
    handle.emit_transient(Event::ProviderResponseFinishedReported(finished(
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
    )))
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
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

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
    fn binding_user_text(&self) -> Option<&str> {
        match self {
            Self::Text { user_text, .. }
            | Self::CompactedText {
                user_text,
                summary: _,
                removed_user_text: _,
                response: _,
            }
            | Self::DummyToolCall { user_text, .. }
            | Self::DummyToolResult { user_text, .. }
            | Self::DummyToolRepair { user_text, .. }
            | Self::AgentStartCall { user_text, .. }
            | Self::AgentStartResult { user_text, .. }
            | Self::AgentWatchCall { user_text, .. }
            | Self::AgentWatchResult { user_text, .. }
            | Self::CoreShellWorkdirCall { user_text, .. }
            | Self::CoreShellWorkdirResult { user_text, .. }
            | Self::CoreShellCreateResult { user_text, .. }
            | Self::CoreShellResumeEditCall { user_text, .. }
            | Self::CoreShellResumeEditResult { user_text, .. }
            | Self::Error { user_text, .. }
            | Self::HoldUntilCancel { user_text, .. }
            | Self::Disconnect { user_text, .. }
            | Self::BarrierText { user_text, .. } => Some(user_text),
            Self::StandaloneCompaction { summary: _ }
            | Self::StandaloneCompactionError {
                failure_kind: _,
                error: _,
            }
            | Self::StandaloneCompactionHold { timeout_ms: _ }
            | Self::WatchNotifications { .. }
            | Self::WatchNotificationChains { .. } => None,
        }
    }

    /// Returns whether this closed action admits the provider operation it
    /// explicitly models.
    fn matches_operation(&self, operation: tau_proto::PromptOperation) -> bool {
        match self {
            Self::StandaloneCompaction { summary: _ }
            | Self::StandaloneCompactionError {
                failure_kind: _,
                error: _,
            }
            | Self::StandaloneCompactionHold { timeout_ms: _ } => {
                operation == tau_proto::PromptOperation::StandaloneCompaction
            }
            _ => operation.is_inference(),
        }
    }
}

/// Requires one exact synthetic interrupted-tool result in provider context.
fn require_repaired_dummy_result(
    prompt: &tau_proto::AgentPromptCreated,
    call_id: &tau_proto::ToolCallId,
    diagnostic: &str,
) -> Result<(), &'static str> {
    let results = prompt
        .context
        .flatten_iter()
        .filter_map(|item| match item {
            ContextItem::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect::<Vec<_>>();
    if results.len() != 1
        || &results[0].call_id != call_id
        || results[0].tool_type != ToolType::Function
        || results[0].status
            != (tau_proto::ToolResultStatus::Error {
                message: diagnostic.to_owned(),
            })
    {
        return Err("dummy tool repair continuity mismatch");
    }
    Ok(())
}

/// Return exact provider user text under the closed fixture convention.
///
/// Exact canonical `<user>` syntax is reserved for fixture HumanUi projections;
/// the fake does not infer or decode provenance from provider text.
fn scenario_user_texts(prompt: &tau_proto::AgentPromptCreated) -> Vec<String> {
    provider_user_texts(prompt)
}

fn latest_provider_user_text(prompt: &tau_proto::AgentPromptCreated) -> Option<String> {
    provider_user_texts(prompt).pop()
}

fn provider_user_texts(prompt: &tau_proto::AgentPromptCreated) -> Vec<String> {
    prompt
        .context
        .flatten()
        .into_iter()
        .filter_map(|item| match item {
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
        })
        .collect()
}

/// Project fixture-authored typed HumanUi text without attempting inversion.
fn project_fixture_human_ui_user_prompt(text: &str) -> String {
    let body = tau_proto::escape_exact_sentinel_close(text, "</user>", "&lt;/user&gt;");
    format!("<user>{body}</user>")
}

/// Match either raw non-HumanUi text or fixture-authored HumanUi projection.
fn fixture_user_text_matches(actual: &str, expected: &str) -> bool {
    actual == expected || actual == project_fixture_human_ui_user_prompt(expected)
}

fn cbor_map_text_field<'a>(value: &'a CborValue, field: &str) -> Option<&'a str> {
    let CborValue::Map(entries) = value else {
        return None;
    };
    entries.iter().find_map(|(key, value)| match (key, value) {
        (CborValue::Text(key), CborValue::Text(value)) if key == field => Some(value.as_str()),
        _ => None,
    })
}

/// Returns the exact production `agent_watch` input schema.
fn agent_watch_parameters() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "agent_id": {
                "type": "string",
                "maxLength": tau_proto::AGENT_ID_MAX_LEN,
                "pattern": "^[A-Za-z0-9_-]{1,64}$",
                "description": "Agent id to watch or stop watching. Must contain only ASCII letters, digits, `_`, or `-`."
            },
            "enable": {
                "type": "boolean",
                "description": "True to enable watching, false to disable it."
            }
        },
        "required": ["agent_id", "enable"],
        "additionalProperties": false
    })
}

/// Returns the exact production `agent_start` input schema.
fn agent_start_parameters() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "prompt": {
                "type": "string",
                "description": "Initial prompt for the sub-agent."
            },
            "role": {
                "type": "string",
                "description": "Sub-agent role to use."
            }
        },
        "required": ["prompt", "role"],
        "additionalProperties": false
    })
}

fn cbor_map(fields: Vec<(&str, CborValue)>) -> CborValue {
    CborValue::Map(
        fields
            .into_iter()
            .map(|(key, value)| (CborValue::Text(key.to_owned()), value))
            .collect(),
    )
}

fn edit_arguments(start: i64, end: i64, new_text: String, context: &str) -> CborValue {
    cbor_map(vec![
        ("path", CborValue::Text("resume-sentinel.txt".to_owned())),
        (
            "edits",
            CborValue::Array(vec![cbor_map(vec![
                ("start_line", CborValue::Integer(start.into())),
                ("end_line_exclusive", CborValue::Integer(end.into())),
                ("newText", CborValue::Text(new_text)),
                ("context_line", CborValue::Text(context.to_owned())),
            ])]),
        ),
    ])
}

fn emit_tool_call(
    handle: &tau_client::ClientHandle,
    prompt: &tau_proto::AgentPromptCreated,
    call_id: tau_proto::ToolCallId,
    name: &str,
    arguments: CborValue,
) -> ClientResult<()> {
    handle.emit_transient(Event::ProviderResponseFinishedReported(finished(
        prompt,
        vec![ContextItem::ToolCall(ToolCallItem {
            call_id,
            name: ToolName::new(name),
            tool_type: ToolType::Function,
            raw_arguments_json: None,
            arguments,
            responses_envelope: None,
        })],
        ProviderStopReason::ToolCalls,
    )))
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
                child_agents: HashMap::new(),
            });
        };
        let Some(path) = path else {
            return Ok(RestoredScenarioState {
                cursors: vec![0; scenario.lanes.len()],
                agent_lanes: HashMap::new(),
                child_agents: HashMap::new(),
            });
        };
        match File::open(path) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(MAX_CHECKPOINT_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|error| ClientError::handler(format!("read cursor: {error}")))?;
                if bytes.len() as u64 > MAX_CHECKPOINT_BYTES {
                    return Err(ClientError::handler(
                        "cursor checkpoint exceeds 65536 bytes",
                    ));
                }
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
                let mut bound_lanes = path_std_collections::HashSet::new();
                for binding in checkpoint.agent_lanes {
                    if binding.lane_index >= scenario.lanes.len()
                        || !bound_lanes.insert(binding.lane_index)
                        || agent_lanes
                            .insert(binding.agent_id, binding.lane_index)
                            .is_some()
                    {
                        return Err(ClientError::handler(
                            "cursor checkpoint contains invalid agent lane bindings",
                        ));
                    }
                }
                let consumed_start_counts = agent_lanes
                    .iter()
                    .filter_map(|(agent_id, lane_index)| {
                        let count = scenario.lanes[*lane_index]
                            .actions
                            .iter()
                            .enumerate()
                            .filter(|(action_index, action)| {
                                matches!(action, ScenarioActionV2::AgentStartResult { .. })
                                    && checkpoint.cursors[*lane_index] > *action_index
                            })
                            .count();
                        (count != 0).then_some((agent_id.clone(), count))
                    })
                    .collect::<HashMap<_, _>>();
                let consumed_start_count = consumed_start_counts.values().sum::<usize>();
                if checkpoint.child_agents.len() > MAX_AGENT_START_PAIRS
                    || checkpoint.child_agents.len() != consumed_start_count
                {
                    return Err(ClientError::handler(
                        "cursor checkpoint child bindings do not match consumed starts",
                    ));
                }
                let mut ordered_children = HashMap::<
                    tau_proto::AgentId,
                    std::collections::BTreeMap<usize, tau_proto::AgentId>,
                >::new();
                let mut children = path_std_collections::HashSet::new();
                for binding in checkpoint.child_agents {
                    let parent_lane = agent_lanes.get(&binding.parent_agent_id).copied();
                    let consumed_start_count = consumed_start_counts
                        .get(&binding.parent_agent_id)
                        .copied()
                        .unwrap_or_default();
                    let child_lane = agent_lanes.get(&binding.child_agent_id).copied();
                    if binding.parent_agent_id == binding.child_agent_id
                        || binding.start_ordinal >= consumed_start_count
                        || child_lane == parent_lane
                        || !children.insert(binding.child_agent_id.clone())
                        || ordered_children
                            .entry(binding.parent_agent_id)
                            .or_default()
                            .insert(binding.start_ordinal, binding.child_agent_id)
                            .is_some()
                    {
                        return Err(ClientError::handler(
                            "cursor checkpoint contains invalid child agent bindings",
                        ));
                    }
                }
                let mut child_agents = HashMap::new();
                for (parent, count) in consumed_start_counts {
                    let Some(ordered) = ordered_children.remove(&parent) else {
                        return Err(ClientError::handler(
                            "cursor checkpoint omits a consumed child binding",
                        ));
                    };
                    if ordered.len() != count || ordered.keys().copied().ne(0..count) {
                        return Err(ClientError::handler(
                            "cursor checkpoint child ordinals are not contiguous",
                        ));
                    }
                    child_agents.insert(parent, ordered.into_values().collect());
                }
                if !ordered_children.is_empty() {
                    return Err(ClientError::handler(
                        "cursor checkpoint child parent does not own a consumed start",
                    ));
                }
                Ok(RestoredScenarioState {
                    cursors: checkpoint.cursors,
                    agent_lanes,
                    child_agents,
                })
            }
            Err(error) if error.kind() == path_std_io::ErrorKind::NotFound => {
                Ok(RestoredScenarioState {
                    cursors: vec![0; scenario.lanes.len()],
                    agent_lanes: HashMap::new(),
                    child_agents: HashMap::new(),
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
