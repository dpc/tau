//! Closed deterministic provider implementation used only by e2e tests.

use std::{collections as path_std_collections, io as path_std_io};

#[cfg(test)]
mod tests;
mod validation;

use std::collections::{BTreeSet, HashMap, VecDeque};
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
    ToolCallId, ToolCallItem, ToolName, ToolType, Verbosity,
};
use validation::{validate_v1, validate_v2};

use crate::scenario::{
    AgentWatchResultExpectationV2, CANONICAL_OPAQUE_COMPACTION_JSON, FAKE_MODEL_ID,
    InitialStatusOutcome, ScenarioActionV2, ScenarioTurnV1, ScenarioV1, ScenarioV2,
    StatusTerminalPhase, StatusToolOrder, WatchNotificationV2,
};

const MAX_TURNS: usize = 8;
const MAX_SCENARIO_BYTES: usize = 16 * 1024;
const MAX_CHECKPOINT_BYTES: u64 = 64 * 1024;
const STATUS_REMINDER: &str = "Set your status to `working` before continuing substantive tool work. Batch the `status` call with other tool calls when possible.";
const MAX_DELTAS: usize = 8;
const MAX_AGENT_START_PAIRS: usize = 2;

/// Closed public-PTY scenarios allowed to bind their sole lane from the
/// harness-minted interactive prompt correlation.
///
/// See `SPEC-tau-e2e-deterministic-provider`.
const PUBLIC_PTY_DYNAMIC_LANE_SCENARIOS: &[&str] = &[
    "spawned-tau-cold-resume",
    "live-dual-pty-attach",
    "prompt-stdin-literal-colon",
    "prompt-stdin-piped-terminal-controls",
    "prompt-stdin-pty-terminal-controls",
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
    /// Return whether this scenario emits parallel tool calls.
    fn supports_parallel_tool_calls(&self) -> bool {
        matches!(self, Self::V1(scenario) if scenario.uses_status_policy())
    }

    /// Returns whether the closed scenario explicitly exercises standalone
    /// compaction, the fake's sole opt-in capability expansion.
    fn enables_standalone_compaction(&self) -> bool {
        matches!(
            self,
            Self::V2(scenario)
                if scenario.lanes.iter().flat_map(|lane| &lane.actions).any(|action| {
                    matches!(
                        action,
                            ScenarioActionV2::StandaloneCompaction { narrative: _ }
                            | ScenarioActionV2::StandaloneOpaqueCompaction
                            | ScenarioActionV2::ReactiveOpaqueCompaction {
                                removed_user_text: _,
                                removed_assistant_text: _,
                                overflow_user_text: _,
                            }
                            | ScenarioActionV2::StandaloneCompactionError {
                                failure_kind: _,
                                error: _,
                            }
                            | ScenarioActionV2::StandaloneCompactionHold { timeout_ms: _ }
                    )
                })
        )
    }

    /// Returns whether the closed scenario needs the typed-image route.
    fn enables_typed_image(&self) -> bool {
        matches!(
            self,
            Self::V2(scenario)
                if scenario.lanes.iter().flat_map(|lane| &lane.actions).any(|action| {
                    matches!(action, ScenarioActionV2::TypedImageToolCall { .. })
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
    /// Test-driver-created idle workers keyed by their durable parent.
    idle_workers: HashMap<tau_proto::AgentId, tau_proto::AgentId>,
    /// Exact message recipients selected by the closed message call.
    message_recipients: HashMap<tau_proto::AgentId, tau_proto::AgentId>,
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
    /// Lane-local provider output released when every participant arrives.
    output: BarrierOutput,
}

/// Closed provider output released for one barrier participant.
enum BarrierOutput {
    /// One ordinary assistant text response.
    Text(String),
    /// One tool-calling response containing parallel dummy calls.
    ParallelDummyTools(Vec<tau_proto::ToolCallId>),
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
                let supports_parallel_tool_calls =
                    cx.config.scenario.supports_parallel_tool_calls();
                let supports_typed_image = cx.config.scenario.enables_typed_image();
                cx.state.scenario = Some(cx.config.scenario);
                cx.state.lane_cursors = restored.cursors;
                cx.state.agent_lanes = restored.agent_lanes;
                cx.state.child_agents = restored.child_agents;
                cx.state.checkpoint = checkpoint;
                cx.state.trace = Some(Arc::new(Mutex::new(trace)));
                cx.handle
                    .emit_transient(Event::ProviderModelsDeclared(model_snapshot(
                        FakeModelCapabilities {
                            standalone_compaction: supports_standalone_compaction,
                            parallel_tool_calls: supports_parallel_tool_calls,
                            typed_image: supports_typed_image,
                        },
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
                tau_proto::EventSelector::Exact(EventName::AGENT_STARTED),
                |cx| {
                    let Event::AgentStarted(started) = cx.delivery.event() else {
                        return Ok(());
                    };
                    cx.state.record_idle_worker(started)
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

/// Closed optional model capabilities derived from the configured scenario.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct FakeModelCapabilities {
    /// Whether standalone compaction prompts are accepted.
    standalone_compaction: bool,
    /// Whether one provider response may contain parallel calls.
    parallel_tool_calls: bool,
    /// Whether this route accepts typed image tool-result content.
    typed_image: bool,
}

fn model_snapshot(capabilities: FakeModelCapabilities) -> ProviderModelsDeclared {
    ProviderModelsDeclared {
        models: vec![ProviderModelInfo {
            id: FAKE_MODEL_ID.into(),
            display_name: Some("Deterministic test model".to_owned()),
            tags: Vec::new(),
            supported_tool_types: vec![ToolType::Function],
            input_modalities: if capabilities.typed_image {
                vec![InputModality::Text, InputModality::Image]
            } else {
                vec![InputModality::Text]
            },
            tool_result_modalities: if capabilities.typed_image {
                vec![InputModality::Image]
            } else {
                Vec::new()
            },
            supports_parallel_tool_calls: capabilities.parallel_tool_calls,
            default_affinity: 0,
            context_window: 16_384,
            efforts: vec![Effort::Off],
            verbosities: vec![Verbosity::Low],
            thinking_summaries: vec![ThinkingSummary::Off],
            supports_compaction: false,
            supports_standalone_compaction: capabilities.standalone_compaction,
            standalone_compaction_threshold: None,
            cache_policy: None,
            est_uncached_input_cost_1m_usd: Some(
                tau_proto::EstimatedUsdPerMillion::from_micro_usd(2_000_000),
            ),
            est_cached_input_cost_1m_usd: Some(tau_proto::EstimatedUsdPerMillion::from_micro_usd(
                1_000_000,
            )),
            est_cache_write_input_cost_1m_usd: Default::default(),
            est_output_cost_1m_usd: Some(tau_proto::EstimatedUsdPerMillion::from_micro_usd(
                4_000_000,
            )),
            est_cache_storage_cost_1m_token_hour_usd: None,
        }],
    }
}

impl FakeState {
    /// Records the sole test-driver-created idle worker without accepting a
    /// production `agent_start` worker as fixture authority.
    fn record_idle_worker(&mut self, started: &tau_proto::AgentStarted) -> ClientResult<()> {
        let message_scenario = matches!(
            self.scenario.as_ref(),
            Some(ScenarioConfig::V2(scenario))
                if scenario.lanes.iter().flat_map(|lane| &lane.actions).any(
                    |action| matches!(action, ScenarioActionV2::MessageCall { .. })
                )
        );
        if !message_scenario {
            return Ok(());
        }
        let Some(parent) = started.parent_agent.as_ref() else {
            return Ok(());
        };
        if self
            .idle_workers
            .insert(parent.clone(), started.agent_id.clone())
            .is_some()
        {
            return Err(ClientError::handler("message worker parent was duplicated"));
        }
        Ok(())
    }

    /// Returns the child owned by the parent's most recently consumed start.
    fn current_child_agent(
        &mut self,
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
                    tau_internal_envelope(&format!(
                        "Watched agent {child_agent_id} emitted a response\n\n\
                         <response>\n{body}\n</response>"
                    ))
                }
                WatchNotificationV2::Prompt { content } => {
                    let body = tau_proto::escape_exact_sentinel_close(
                        content,
                        "</prompt>",
                        "&lt;/prompt&gt;",
                    );
                    tau_internal_envelope(&format!(
                        "Watched agent {child_agent_id} received a user prompt\n\n\
                         <prompt>\n{body}\n</prompt>"
                    ))
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

    fn status_policy_tool_call(
        &mut self,
        index: usize,
        prompt: &tau_proto::AgentPromptCreated,
        user_text: &str,
        order: StatusToolOrder,
        initial_status: InitialStatusOutcome,
    ) -> ClientResult<Event> {
        self.require_human_ui_user_text(index, prompt, user_text)?;
        let tool_names = prompt
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();
        if tool_names.len() != 2
            || !tool_names.contains(&"status")
            || !tool_names.contains(&tau_ext_test_dummy::RESTART_TEST_DUMMY_TOOL_NAME)
        {
            return Err(self.mismatch(
                index,
                &format!("status-policy tool snapshot mismatch: {tool_names:?}"),
            ));
        }
        let status = status_call(
            "status-policy-working",
            match initial_status {
                InitialStatusOutcome::AcceptedWorking => "working",
                InitialStatusOutcome::Rejected => "invalid",
            },
            "Exercise current status policy",
        );
        let work = ContextItem::ToolCall(ToolCallItem {
            call_id: "status-policy-work".into(),
            name: ToolName::new(tau_ext_test_dummy::RESTART_TEST_DUMMY_TOOL_NAME),
            tool_type: ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: Some("{}".to_owned()),
            responses_envelope: None,
        });
        let output_items = match order {
            StatusToolOrder::StatusFirst => vec![status, work],
            StatusToolOrder::WorkFirst => vec![work, status],
        };
        Ok(Event::ProviderResponseFinishedReported(finished(
            prompt,
            output_items,
            ProviderStopReason::ToolCalls,
        )))
    }

    fn status_policy_tool_result(
        &mut self,
        index: usize,
        prompt: &tau_proto::AgentPromptCreated,
        initial_status: InitialStatusOutcome,
    ) -> ClientResult<Event> {
        require_tool_result(prompt, "status-policy-work", "restart succeeded")
            .map_err(|message| self.mismatch(index, message))?;
        match initial_status {
            InitialStatusOutcome::AcceptedWorking => {
                require_tool_result(
                    prompt,
                    "status-policy-working",
                    "Status accepted: working — Exercise current status policy",
                )
                .map_err(|message| self.mismatch(index, message))?;
                if prompt_contains_text(prompt, STATUS_REMINDER) {
                    return Err(self.mismatch(index, "Working received a start reminder"));
                }
            }
            InitialStatusOutcome::Rejected => {
                require_tool_error(prompt, "status-policy-working")
                    .map_err(|message| self.mismatch(index, message))?;
                if !prompt_contains_text(prompt, STATUS_REMINDER) {
                    return Err(self.mismatch(index, "rejected status suppressed reminder"));
                }
            }
        }
        let continuation = match initial_status {
            InitialStatusOutcome::AcceptedWorking => ContextItem::ToolCall(ToolCallItem {
                call_id: "status-policy-followup-work".into(),
                name: ToolName::new(tau_ext_test_dummy::RESTART_TEST_DUMMY_TOOL_NAME),
                tool_type: ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: Some("{}".to_owned()),
                responses_envelope: None,
            }),
            InitialStatusOutcome::Rejected => status_call(
                "status-policy-recovery-working",
                "working",
                "Exercise current status policy",
            ),
        };
        Ok(Event::ProviderResponseFinishedReported(finished(
            prompt,
            vec![continuation],
            ProviderStopReason::ToolCalls,
        )))
    }

    fn working_followup_tool_result(
        &mut self,
        index: usize,
        prompt: &tau_proto::AgentPromptCreated,
        initial_status: InitialStatusOutcome,
    ) -> ClientResult<Event> {
        match initial_status {
            InitialStatusOutcome::AcceptedWorking => {
                require_tool_result(prompt, "status-policy-followup-work", "restart succeeded")
            }
            InitialStatusOutcome::Rejected => require_tool_result(
                prompt,
                "status-policy-recovery-working",
                "Status accepted: working — Exercise current status policy",
            ),
        }
        .map_err(|message| self.mismatch(index, message))?;
        let expected_reminders = match initial_status {
            InitialStatusOutcome::AcceptedWorking => 0,
            InitialStatusOutcome::Rejected => 1,
        };
        if prompt_text_occurrences(prompt, STATUS_REMINDER) != expected_reminders {
            return Err(self.mismatch(index, "Working reminder count changed"));
        }
        Ok(Event::ProviderResponseFinishedReported(finished(
            prompt,
            vec![assistant_message(
                "premature final while Working".to_owned(),
            )],
            ProviderStopReason::EndTurn,
        )))
    }

    fn working_final_status_call(
        &mut self,
        index: usize,
        prompt: &tau_proto::AgentPromptCreated,
        terminal_phase: StatusTerminalPhase,
    ) -> ClientResult<Event> {
        let expected = "Your `status` is set to `working` on \"Exercise current status policy\". Set it to `done`, `waiting`, or `blocked` to finish or call `wait` when waiting for external events.";
        if !prompt_contains_text(prompt, expected) {
            return Err(self.mismatch(index, "Working final challenge is absent"));
        }
        Ok(Event::ProviderResponseFinishedReported(finished(
            prompt,
            vec![status_call(
                "status-policy-terminal",
                terminal_phase.as_str(),
                "Exercise current status policy",
            )],
            ProviderStopReason::ToolCalls,
        )))
    }

    fn terminal_status_result(
        &mut self,
        index: usize,
        prompt: &tau_proto::AgentPromptCreated,
        terminal_phase: StatusTerminalPhase,
        response: String,
    ) -> ClientResult<Event> {
        let expected = format!(
            "Status accepted: {} — Exercise current status policy",
            terminal_phase.as_str()
        );
        require_tool_result(prompt, "status-policy-terminal", &expected)
            .map_err(|message| self.mismatch(index, message))?;
        Ok(Event::ProviderResponseFinishedReported(finished(
            prompt,
            vec![assistant_message(response)],
            ProviderStopReason::EndTurn,
        )))
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
            ScenarioTurnV1::StatusPolicyToolCall {
                user_text,
                order,
                initial_status,
                terminal_phase: _,
            } => self.status_policy_tool_call(index, prompt, &user_text, order, initial_status)?,
            ScenarioTurnV1::StatusPolicyToolResult {
                initial_status,
                terminal_phase: _,
            } => self.status_policy_tool_result(index, prompt, initial_status)?,
            ScenarioTurnV1::WorkingFollowupToolResult {
                initial_status,
                terminal_phase: _,
            } => self.working_followup_tool_result(index, prompt, initial_status)?,
            ScenarioTurnV1::WorkingFinalStatusCall { terminal_phase } => {
                self.working_final_status_call(index, prompt, terminal_phase)?
            }
            ScenarioTurnV1::TerminalStatusResult {
                terminal_phase,
                response,
            } => self.terminal_status_result(index, prompt, terminal_phase, response)?,
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
                .message_recipients
                .values()
                .any(|id| id == &prompt.agent_id)
                && !self
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
                        && (matches!(
                            lane.actions.first(),
                            Some(ScenarioActionV2::MessageInbound { .. })
                        ) || lane
                            .actions
                            .first()
                            .and_then(ScenarioActionV2::binding_user_text)
                            .is_some_and(|expected| {
                                actual.as_deref().is_some_and(|actual| {
                                    fixture_user_text_matches(actual, expected)
                                })
                            }))
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
            ScenarioActionV2::OutputLengthReasoning {
                reasoning,
                report_usage,
                ..
            } => emit_output_length_reasoning(prompt, handle, reasoning, report_usage),
            ScenarioActionV2::OutputLengthContinuation {
                response,
                report_usage,
                ..
            } => {
                let mut finished = finished(
                    prompt,
                    vec![assistant_message(response)],
                    ProviderStopReason::EndTurn,
                );
                finished.backend = Some(tau_proto::ProviderBackend {
                    kind: tau_proto::ProviderBackendKind::ChatCompletions,
                    base_url: "https://deterministic.invalid/v1".to_owned(),
                    transport: tau_proto::ProviderBackendTransport::HttpSse,
                    stale_chain_fallback: false,
                });
                finished.provider_response_id = Some("resp-output-length-successor".to_owned());
                if report_usage {
                    finished.usage = Some(output_length_usage(OutputLengthUsagePhase::Successor));
                }
                handle.emit_transient(Event::ProviderResponseFinishedReported(finished))
            }
            ScenarioActionV2::TextWithUsage { response, .. } => {
                let mut finished = finished(
                    prompt,
                    vec![assistant_message(response)],
                    ProviderStopReason::EndTurn,
                );
                finished.usage = Some(tau_proto::ProviderTokenUsage {
                    prompt_sent_tokens: 2_000,
                    prompt_cached_tokens: 0,
                    response_received_tokens: 1,
                    ..Default::default()
                });
                handle.emit_transient(Event::ProviderResponseFinishedReported(finished))
            }
            ScenarioActionV2::DummyToolResultWithUsage { response, .. } => {
                let mut finished = finished(
                    prompt,
                    vec![assistant_message(response)],
                    ProviderStopReason::EndTurn,
                );
                finished.usage = Some(tau_proto::ProviderTokenUsage {
                    prompt_sent_tokens: 2_000,
                    prompt_cached_tokens: 0,
                    response_received_tokens: 1,
                    ..Default::default()
                });
                handle.emit_transient(Event::ProviderResponseFinishedReported(finished))
            }
            ScenarioActionV2::Text { response, .. }
            | ScenarioActionV2::CompactedText { response, .. }
            | ScenarioActionV2::CompactedOpaqueText { response, .. }
            | ScenarioActionV2::ReactiveCompactedOpaqueText { response, .. }
            | ScenarioActionV2::DummyToolResult { response, .. }
            | ScenarioActionV2::TypedImageToolResult { response, .. }
            | ScenarioActionV2::TypedImageReplay { response, .. }
            | ScenarioActionV2::DummyToolRepair { response, .. }
            | ScenarioActionV2::MessageSenderResult { response, .. }
            | ScenarioActionV2::MessageInbound { response, .. }
            | ScenarioActionV2::MessageInboundAfterHeld { response, .. }
            | ScenarioActionV2::ProviderContextRawMessageResult { response, .. }
            | ScenarioActionV2::MessageAndRawInboundAfterHeld { response, .. }
            | ScenarioActionV2::MessageAndRawInboundAfterParallelTools { response, .. }
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
            ScenarioActionV2::StandaloneCompaction { narrative } => {
                emit_local_compaction_narrative(prompt, handle, narrative)
            }
            ScenarioActionV2::StandaloneOpaqueCompaction => {
                emit_opaque_compaction_response(prompt, handle)
            }
            ScenarioActionV2::ReactiveOpaqueCompaction {
                removed_user_text: _,
                removed_assistant_text: _,
                overflow_user_text: _,
            } => emit_opaque_compaction_response(prompt, handle),
            ScenarioActionV2::StandaloneCompactionError {
                failure_kind,
                error,
            }
            | ScenarioActionV2::Error {
                failure_kind,
                error,
                ..
            } => emit_error_response(prompt, handle, failure_kind, error),
            ScenarioActionV2::ContextOverflow {
                user_text: _,
                failure_kind,
                ..
            } => emit_error_response(
                prompt,
                handle,
                failure_kind,
                "synthetic canonical context-window rejection".to_owned(),
            ),
            ScenarioActionV2::StandaloneCompactionHold { timeout_ms } => {
                self.emit_hold_until_cancel(prompt, handle, timeout_ms)
            }
            ScenarioActionV2::DummyToolCall { call_id, .. } => emit_dummy_tool_call(
                prompt,
                handle,
                call_id,
                tau_ext_test_dummy::RESTART_TEST_DUMMY_TOOL_NAME,
            ),
            ScenarioActionV2::ProviderContextRawMessageCall {
                call_id, raw_text, ..
            } => {
                let recipient = self
                    .message_recipients
                    .get(&prompt.agent_id)
                    .cloned()
                    .ok_or_else(|| self.mismatch(0, "raw message tool has no recipient"))?;
                emit_tool_call(
                    handle,
                    prompt,
                    call_id,
                    tau_ext_test_dummy::PROVIDER_CONTEXT_RAW_MESSAGE_TOOL_NAME,
                    cbor_map(vec![
                        ("agent_id", CborValue::Text(recipient.to_string())),
                        ("text", CborValue::Text(raw_text)),
                    ]),
                )
            }
            ScenarioActionV2::TypedImageToolCall { call_id, .. } => emit_dummy_tool_call(
                prompt,
                handle,
                call_id,
                tau_ext_test_dummy::TYPED_IMAGE_TEST_DUMMY_TOOL_NAME,
            ),
            action @ (ScenarioActionV2::AgentStartCall { .. }
            | ScenarioActionV2::MessageCall { .. }
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
            ScenarioActionV2::BarrierParallelDummyTools {
                barrier,
                participants,
                tool_call_ids,
                ..
            } => self.emit_barrier(
                prompt,
                handle,
                barrier,
                participants,
                BarrierOutput::ParallelDummyTools(tool_call_ids),
            ),
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
        self.emit_barrier(
            prompt,
            handle,
            barrier,
            participants,
            BarrierOutput::Text(response),
        )
    }

    /// Queues one typed participant output and releases all complete-barrier
    /// outputs in arrival order.
    fn emit_barrier(
        &mut self,
        prompt: &tau_proto::AgentPromptCreated,
        handle: &tau_client::ClientHandle,
        barrier: String,
        participants: usize,
        output: BarrierOutput,
    ) -> ClientResult<()> {
        let pending = self.barriers.entry(barrier.clone()).or_default();
        pending.push(BarrierParticipant {
            prompt: prompt.clone(),
            output,
        });
        if participants < pending.len() {
            return Err(ClientError::handler("barrier over-subscribed"));
        }
        if pending.len() == participants {
            let completed = self.barriers.remove(&barrier).unwrap_or_default();
            for participant in completed {
                match participant.output {
                    BarrierOutput::Text(response) => {
                        emit_text_response(&participant.prompt, handle, response)?;
                    }
                    BarrierOutput::ParallelDummyTools(call_ids) => {
                        emit_parallel_dummy_tool_calls(&participant.prompt, handle, call_ids)?;
                    }
                }
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
            ScenarioActionV2::MessageCall {
                call_id, message, ..
            } => {
                let Some(recipient_id) = self.idle_workers.get(&prompt.agent_id) else {
                    return Err(
                        self.mismatch(0, "message call has no test-driver-created idle worker")
                    );
                };
                self.message_recipients
                    .insert(prompt.agent_id.clone(), recipient_id.clone());
                emit_tool_call(
                    handle,
                    prompt,
                    call_id,
                    "message",
                    cbor_map(vec![
                        ("recipient_id", CborValue::Text(recipient_id.to_string())),
                        ("message", CborValue::Text(message)),
                    ]),
                )
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

    /// Returns the first repaired call only for the sole closed two-pair
    /// disconnect/replacement scenario.
    fn exit_once_repair_before(&self, cursor: usize) -> Option<(ToolCallId, String)> {
        let scenario = self.v2().ok()?;
        let actions = scenario.lanes.first()?.actions.as_slice();
        let [
            ScenarioActionV2::DummyToolCall {
                call_id: first_call,
                ..
            },
            ScenarioActionV2::DummyToolRepair {
                call_id: repair_call,
                diagnostic,
                ..
            },
            ScenarioActionV2::DummyToolCall { .. },
            ScenarioActionV2::DummyToolResult { .. },
        ] = actions
        else {
            return None;
        };
        (scenario.lanes.len() == 1 && cursor == 3 && first_call == repair_call)
            .then_some((first_call.clone(), diagnostic.clone()))
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
            ScenarioActionV2::OutputLengthContinuation {
                user_text,
                reasoning,
                ..
            } => {
                validate_output_length_continuation_context(prompt, user_text, reasoning)
                    .map_err(|message| self.mismatch(cursor, message))?;
            }
            ScenarioActionV2::DummyToolCall { .. }
            | ScenarioActionV2::TypedImageToolCall { .. } => {
                let tool_name = if matches!(action, ScenarioActionV2::TypedImageToolCall { .. }) {
                    tau_ext_test_dummy::TYPED_IMAGE_TEST_DUMMY_TOOL_NAME
                } else {
                    tau_ext_test_dummy::RESTART_TEST_DUMMY_TOOL_NAME
                };
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
            ScenarioActionV2::DummyToolResult { call_id, .. }
            | ScenarioActionV2::DummyToolResultWithUsage { call_id, .. } => {
                let results = prompt
                    .context
                    .flatten_iter()
                    .filter_map(|item| match item {
                        ContextItem::ToolResult(result) => Some(result),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                if let Some((first_call_id, first_diagnostic)) =
                    self.exit_once_repair_before(cursor)
                {
                    if results.len() != 2
                        || !results.iter().any(|result| {
                            result.call_id == first_call_id
                                && result.tool_type == ToolType::Function
                                && result.status
                                    == tau_proto::ToolResultStatus::Error {
                                        message: first_diagnostic.to_owned(),
                                    }
                        })
                        || !results.iter().any(|result| {
                            result.call_id == *call_id
                                && result.tool_type == ToolType::Function
                                && result.status == tau_proto::ToolResultStatus::Success
                                && result.output.body == "restart succeeded"
                        })
                    {
                        return Err(self.mismatch(
                            cursor,
                            "exit-once dummy continuation must retain exactly repaired error and replacement success",
                        ));
                    }
                } else if results.len() != 1
                    || results[0].call_id != *call_id
                    || results[0].tool_type != ToolType::Function
                    || results[0].status != tau_proto::ToolResultStatus::Success
                    || results[0].output.body != "restart succeeded"
                {
                    return Err(self.mismatch(cursor, "dummy tool result continuity mismatch"));
                }
            }
            ScenarioActionV2::ProviderContextRawMessageCall { .. } => {
                let names = prompt
                    .tools
                    .iter()
                    .map(|tool| tool.name.as_str())
                    .collect::<BTreeSet<_>>();
                if names
                    != BTreeSet::from([
                        "message",
                        tau_ext_test_dummy::PROVIDER_CONTEXT_RAW_MESSAGE_TOOL_NAME,
                    ])
                {
                    return Err(self.mismatch(cursor, "raw message tool snapshot mismatch"));
                }
            }
            ScenarioActionV2::ProviderContextRawMessageResult { call_id, .. } => {
                let matches = prompt
                    .context
                    .flatten_iter()
                    .filter(|item| {
                        matches!(
                            item,
                            ContextItem::ToolResult(result)
                                if result.call_id == *call_id
                                    && result.status == tau_proto::ToolResultStatus::Success
                                    && result.output.body == "raw message emitted"
                        )
                    })
                    .count();
                if matches != 1 {
                    return Err(self.mismatch(cursor, "raw message tool result mismatch"));
                }
            }
            ScenarioActionV2::TypedImageToolResult { call_id, .. }
            | ScenarioActionV2::TypedImageReplay { call_id, .. } => {
                require_typed_image_tool_result(prompt, call_id)
                    .map_err(|detail| self.mismatch(cursor, detail))?;
            }
            ScenarioActionV2::DummyToolRepair {
                call_id,
                diagnostic,
                ..
            } => {
                require_repaired_dummy_result(prompt, call_id, diagnostic)
                    .map_err(|detail| self.mismatch(cursor, detail))?;
            }
            ScenarioActionV2::MessageCall { .. } => {
                let message = prompt
                    .tools
                    .iter()
                    .find(|tool| tool.name.as_str() == "message");
                if message.is_none_or(|tool| tool.tool_type != ToolType::Function) {
                    self.trace(&format!(
                        "message tools={}",
                        serde_json::to_string(&prompt.tools)
                            .unwrap_or_else(|_| "<unserializable>".to_owned())
                    ))?;
                    return Err(self.mismatch(cursor, "message tool snapshot mismatch"));
                }
            }
            ScenarioActionV2::MessageSenderResult {
                call_id, message, ..
            } => {
                let results = prompt
                    .context
                    .flatten_iter()
                    .filter_map(|item| match item {
                        ContextItem::ToolResult(result) => Some(result),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                let committed_message_id = results.first().and_then(|result| {
                    result
                        .output
                        .body
                        .strip_prefix("Message committed: ")
                        .and_then(|body| {
                            body.strip_suffix("; recipient was live; response not guaranteed")
                        })
                });
                if results.len() != 1
                    || results[0].call_id != *call_id
                    || results[0].tool_type != ToolType::Function
                    || results[0].status != tau_proto::ToolResultStatus::Success
                    || committed_message_id.is_none_or(|message_id| {
                        tau_proto::AgentMessageId::parse(message_id).is_err()
                    })
                    || prompt.context.flatten_iter().any(|item| {
                        matches!(
                            item,
                            ContextItem::Message(message_item)
                                if message_item.content.iter().any(|part| match part {
                                    ContentPart::Text { text }
                                    | ContentPart::HarnessInternalText { text } => text.contains(message),
                                })
                        )
                    })
                {
                    return Err(self.mismatch(
                        cursor,
                        "message sender continuation did not contain one compact result",
                    ));
                }
            }
            action @ (ScenarioActionV2::MessageInbound {
                call_id, message, ..
            }
            | ScenarioActionV2::MessageInboundAfterHeld {
                call_id, message, ..
            }
            | ScenarioActionV2::MessageAndRawInboundAfterHeld {
                call_id, message, ..
            }
            | ScenarioActionV2::MessageAndRawInboundAfterParallelTools {
                call_id,
                message,
                ..
            }) => {
                let restored_sender = (|| {
                    let Some(ScenarioConfig::V2(scenario)) = self.scenario.as_ref() else {
                        return None;
                    };
                    let sender_lane = scenario.lanes.iter().position(|lane| {
                        lane.actions.iter().any(|action| {
                            matches!(
                                action,
                                ScenarioActionV2::MessageCall { call_id: candidate, .. }
                                    if candidate == call_id
                            )
                        })
                    })?;
                    self.agent_lanes.iter().find_map(|(agent_id, lane)| {
                        (*lane == sender_lane).then(|| agent_id.clone())
                    })
                })();
                let sender = self
                    .message_recipients
                    .iter()
                    .find_map(|(sender, recipient)| {
                        (recipient == &prompt.agent_id).then(|| sender.clone())
                    })
                    .or(restored_sender)
                    .ok_or_else(|| self.mismatch(cursor, "message worker was not selected"))?;
                let expected = format!(
                    "<tau_internal>You have received a message from {sender}\n\n<message>\n{message}\n</message></tau_internal>"
                );
                let texts = provider_user_texts(prompt);
                let exact = match action {
                    ScenarioActionV2::MessageInboundAfterHeld { held_user_text, .. } => {
                        texts == vec![format!("<user>{held_user_text}</user>"), expected.clone()]
                    }
                    ScenarioActionV2::MessageAndRawInboundAfterHeld {
                        held_user_text,
                        raw_text,
                        ..
                    }
                    | ScenarioActionV2::MessageAndRawInboundAfterParallelTools {
                        held_user_text,
                        raw_text,
                        ..
                    } => {
                        let raw = format!(
                            "<message event=\"created\" publisher=\"e2e-test-dummy\" \
                              message_ref=\"provider-context-raw-message\" \
                              sender_ref=\"provider-context-raw-sender\" \
                              content_trust=\"external\">{raw_text}</message>"
                        );
                        texts
                            == vec![
                                format!("<user>{held_user_text}</user>"),
                                expected.clone(),
                                raw,
                            ]
                    }
                    ScenarioActionV2::MessageInbound { .. } => texts == vec![expected.clone()],
                    _ => unreachable!("matched inbound action"),
                };
                if !exact {
                    self.trace(&format!(
                        "message action={action:?} expected={expected:?} actual={texts:?}"
                    ))?;
                    return Err(self.mismatch(
                        cursor,
                        "message worker did not receive exactly one canonical inbound wrapper",
                    ));
                }
                if let ScenarioActionV2::MessageAndRawInboundAfterParallelTools {
                    tool_call_ids,
                    ..
                } = action
                {
                    validate_complete_parallel_dummy_round(prompt, tool_call_ids)
                        .map_err(|message| self.mismatch(cursor, message))?;
                }
            }
            ScenarioActionV2::StandaloneCompaction { narrative: _ }
            | ScenarioActionV2::StandaloneOpaqueCompaction
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
            ScenarioActionV2::ReactiveOpaqueCompaction {
                removed_user_text,
                removed_assistant_text,
                overflow_user_text,
            } => {
                if prompt.context.blocks.is_empty()
                    || !context_has_text(prompt, ContextRole::User, removed_user_text)
                    || !context_has_text(prompt, ContextRole::Assistant, removed_assistant_text)
                    || context_has_text(prompt, ContextRole::User, overflow_user_text)
                {
                    return Err(self.mismatch(
                        cursor,
                        "reactive compaction request did not preserve the closed pre-cut round",
                    ));
                }
            }
            ScenarioActionV2::CompactedText {
                user_text: _,
                checkpoint,
                removed_user_text,
                response: _,
            } => {
                if !context_has_text(prompt, ContextRole::User, checkpoint)
                    || context_has_text(prompt, ContextRole::User, removed_user_text)
                {
                    return Err(self.mismatch(
                        cursor,
                        "exact composite checkpoint did not replace prior transcript",
                    ));
                }
            }
            ScenarioActionV2::CompactedOpaqueText {
                user_text: _,
                removed_user_text,
                response: _,
            } => {
                let context = serde_json::to_string(&prompt.context)
                    .map_err(|error| ClientError::handler(error.to_string()))?;
                let opaque_items = prompt
                    .context
                    .flatten_iter()
                    .filter_map(|item| match item {
                        ContextItem::Compaction(compaction) => Some(compaction),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                let retained_source_items = prompt.context.flatten_iter().any(|item| {
                    matches!(item, ContextItem::ToolCall(_) | ContextItem::ToolResult(_))
                        || matches!(
                            item,
                            ContextItem::Message(message)
                                if message.role == ContextRole::Assistant
                        )
                });
                if !matches!(
                    opaque_items.as_slice(),
                    [compaction]
                        if compaction.raw_json.as_deref()
                            == Some(CANONICAL_OPAQUE_COMPACTION_JSON)
                ) || context.contains(removed_user_text)
                    || retained_source_items
                {
                    return Err(self.mismatch(
                        cursor,
                        "canonical opaque replacement did not replace prior transcript",
                    ));
                }
            }
            ScenarioActionV2::ReactiveCompactedOpaqueText {
                removed_user_text,
                removed_assistant_text,
                overflow_user_text,
                response: _,
            } => {
                let opaque_items = prompt
                    .context
                    .flatten_iter()
                    .filter_map(|item| match item {
                        ContextItem::Compaction(compaction) => Some(compaction),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                if !matches!(
                    opaque_items.as_slice(),
                    [compaction]
                        if compaction.raw_json.as_deref()
                            == Some(CANONICAL_OPAQUE_COMPACTION_JSON)
                ) || context_has_text(prompt, ContextRole::User, removed_user_text)
                    || context_has_text(prompt, ContextRole::Assistant, removed_assistant_text)
                    || !context_has_text(prompt, ContextRole::User, overflow_user_text)
                {
                    return Err(self.mismatch(
                        cursor,
                        "reactive opaque replacement did not replace rejected transcript",
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

/// Publishes the exact typed local narrative envelope that the production
/// Chat Completions extension gives the harness after output validation.
fn emit_local_compaction_narrative(
    prompt: &tau_proto::AgentPromptCreated,
    handle: &tau_client::ClientHandle,
    narrative: String,
) -> ClientResult<()> {
    let item = ContextItem::LocalCompactionNarrative(tau_proto::LocalCompactionNarrativeItem {
        narrative,
    });
    handle.emit_transient(Event::ProviderResponseFinishedReported(finished(
        prompt,
        vec![item],
        ProviderStopReason::EndTurn,
    )))
}

/// Publishes one replay-safe reasoning-only output-limit terminal.
fn emit_output_length_reasoning(
    prompt: &tau_proto::AgentPromptCreated,
    handle: &tau_client::ClientHandle,
    reasoning: String,
    report_usage: bool,
) -> ClientResult<()> {
    let mut response = finished(
        prompt,
        vec![ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
            kind: tau_proto::ReasoningTextKind::Full,
            text: reasoning,
        })],
        ProviderStopReason::Length,
    );
    response.backend = Some(tau_proto::ProviderBackend {
        kind: tau_proto::ProviderBackendKind::ChatCompletions,
        base_url: "https://deterministic.invalid/v1".to_owned(),
        transport: tau_proto::ProviderBackendTransport::HttpSse,
        stale_chain_fallback: false,
    });
    response.provider_response_id = Some("resp-output-length-source".to_owned());
    if report_usage {
        response.usage = Some(output_length_usage(OutputLengthUsagePhase::Source));
    }
    handle.emit_transient(Event::ProviderResponseFinishedReported(response))
}

/// Semantic response phase selecting one fixed accounting record.
enum OutputLengthUsagePhase {
    /// Initial response that plans the continuation.
    Source,
    /// Reserved continuation response that closes the sequence.
    Successor,
}

/// Returns fixed, distinct source/successor usage for exact aggregation
/// oracles.
fn output_length_usage(phase: OutputLengthUsagePhase) -> tau_proto::ProviderTokenUsage {
    let (prompt_sent_tokens, prompt_cached_tokens, response_received_tokens) = match phase {
        OutputLengthUsagePhase::Source => (10, 2, 3),
        OutputLengthUsagePhase::Successor => (20, 5, 7),
    };
    tau_proto::ProviderTokenUsage {
        prompt_sent_tokens,
        prompt_cached_tokens,
        response_received_tokens,
        ..Default::default()
    }
}

/// Publishes the fixed raw provider item used to exercise opaque compaction
/// persistence and replay without interpreting its provider-owned fields.
fn emit_opaque_compaction_response(
    prompt: &tau_proto::AgentPromptCreated,
    handle: &tau_client::ClientHandle,
) -> ClientResult<()> {
    handle.emit_transient(Event::ProviderResponseFinishedReported(finished(
        prompt,
        vec![ContextItem::Compaction(
            tau_proto::OpaqueProviderItem::with_raw_json(
                CborValue::Map(Vec::new()),
                CANONICAL_OPAQUE_COMPACTION_JSON,
            ),
        )],
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
    tool_name: &str,
) -> ClientResult<()> {
    let tool_name = ToolName::new(tool_name);
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

/// Publishes one tool-calling response containing the exact parallel dummy
/// invocations.
fn emit_parallel_dummy_tool_calls(
    prompt: &tau_proto::AgentPromptCreated,
    handle: &tau_client::ClientHandle,
    call_ids: Vec<tau_proto::ToolCallId>,
) -> ClientResult<()> {
    let output_items = call_ids
        .into_iter()
        .map(|call_id| {
            ContextItem::ToolCall(ToolCallItem {
                call_id,
                name: ToolName::new(tau_ext_test_dummy::RESTART_TEST_DUMMY_TOOL_NAME),
                tool_type: ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: Some("{}".to_owned()),
                responses_envelope: None,
            })
        })
        .collect();
    handle.emit_transient(Event::ProviderResponseFinishedReported(finished(
        prompt,
        output_items,
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

/// Build one deterministic status call.
fn status_call(call_id: &str, state: &str, task_name: &str) -> ContextItem {
    ContextItem::ToolCall(ToolCallItem {
        call_id: call_id.into(),
        name: ToolName::new("status"),
        tool_type: ToolType::Function,
        arguments: CborValue::Map(vec![
            (
                CborValue::Text("state".to_owned()),
                CborValue::Text(state.to_owned()),
            ),
            (
                CborValue::Text("task_name".to_owned()),
                CborValue::Text(task_name.to_owned()),
            ),
        ]),
        raw_arguments_json: None,
        responses_envelope: None,
    })
}

/// Require one exact successful tool result in the current prompt.
fn require_tool_result(
    prompt: &tau_proto::AgentPromptCreated,
    call_id: &str,
    body: &str,
) -> Result<(), &'static str> {
    let matches = prompt
        .context
        .flatten_iter()
        .filter_map(|item| match item {
            ContextItem::ToolResult(result) => Some(result),
            _ => None,
        })
        .filter(|result| {
            result.call_id.as_str() == call_id
                && result.status == tau_proto::ToolResultStatus::Success
                && result.output.body == body
        })
        .count();
    (matches == 1)
        .then_some(())
        .ok_or("successful tool result mismatch")
}

/// Requires the one fixed image-bearing dummy result in a provider prompt.
fn require_typed_image_tool_result(
    prompt: &tau_proto::AgentPromptCreated,
    call_id: &tau_proto::ToolCallId,
) -> Result<(), &'static str> {
    let results = prompt
        .context
        .flatten_iter()
        .filter_map(|item| match item {
            ContextItem::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect::<Vec<_>>();
    let Some(result) = results.first() else {
        return Err("typed image result is absent");
    };
    if results.len() != 1 {
        return Err("typed image result count mismatch");
    }
    if result.call_id != *call_id {
        return Err("typed image result call identity mismatch");
    }
    if result.tool_type != ToolType::Function {
        return Err("typed image result tool type mismatch");
    }
    if result.status != tau_proto::ToolResultStatus::Success {
        return Err("typed image result status mismatch");
    }
    if result.output.body != "typed image succeeded" {
        return Err("typed image result text mismatch");
    }
    let [tau_proto::ToolResultContentPart::Image(image)] = result.provider_content.as_slice()
    else {
        return Err("typed image result content shape mismatch");
    };
    if image.media_type != tau_proto::ImageMediaType::Png
        || image.width != 1
        || image.height != 1
        || image.detail != tau_proto::ImageDetail::High
        || image.data.as_ref() != tau_ext_test_dummy::TYPED_IMAGE_PNG
        || blake3::hash(&image.data).to_hex().as_str()
            != "1c22ad7f40a18bbcb1c50dc8a78ac6a1a36b9a0a3c7f9833c965b2ef8100a734"
    {
        return Err("typed image canonical content mismatch");
    }
    Ok(())
}

/// Require one rejected tool result without accepting its diagnostic as status.
fn require_tool_error(
    prompt: &tau_proto::AgentPromptCreated,
    call_id: &str,
) -> Result<(), &'static str> {
    let matches = prompt
        .context
        .flatten_iter()
        .filter_map(|item| match item {
            ContextItem::ToolResult(result) => Some(result),
            _ => None,
        })
        .filter(|result| {
            result.call_id.as_str() == call_id
                && matches!(result.status, tau_proto::ToolResultStatus::Error { .. })
        })
        .count();
    (matches == 1)
        .then_some(())
        .ok_or("rejected tool result mismatch")
}

/// Return whether typed prompt text contains one policy substring.
fn prompt_contains_text(prompt: &tau_proto::AgentPromptCreated, expected: &str) -> bool {
    0 < prompt_text_occurrences(prompt, expected)
}

/// Count typed prompt text items containing one policy substring.
fn prompt_text_occurrences(prompt: &tau_proto::AgentPromptCreated, expected: &str) -> usize {
    prompt
        .context
        .flatten_iter()
        .map(|item| match item {
            ContextItem::Message(message) => message
                .content
                .iter()
                .filter(|part| match part {
                    ContentPart::Text { text } | ContentPart::HarnessInternalText { text } => {
                        text.contains(expected)
                    }
                })
                .count(),
            _ => 0,
        })
        .sum()
}

fn finished(
    prompt: &tau_proto::AgentPromptCreated,
    output_items: Vec<ContextItem>,
    stop_reason: ProviderStopReason,
) -> ProviderResponseFinished {
    ProviderResponseFinished {
        automatic_compaction_decision: None,
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
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        originator: prompt.originator.clone(),
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
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
            | Self::TextWithUsage { user_text, .. }
            | Self::OutputLengthReasoning { user_text, .. }
            | Self::ContextOverflow { user_text, .. }
            | Self::CompactedText {
                user_text,
                checkpoint: _,
                removed_user_text: _,
                response: _,
            }
            | Self::CompactedOpaqueText {
                user_text,
                removed_user_text: _,
                response: _,
            }
            | Self::DummyToolCall { user_text, .. }
            | Self::DummyToolResult { user_text, .. }
            | Self::DummyToolResultWithUsage { user_text, .. }
            | Self::TypedImageToolCall { user_text, .. }
            | Self::TypedImageReplay { user_text, .. }
            | Self::DummyToolRepair { user_text, .. }
            | Self::MessageCall { user_text, .. }
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
            | Self::BarrierText { user_text, .. }
            | Self::BarrierParallelDummyTools { user_text, .. }
            | Self::ProviderContextRawMessageCall { user_text, .. } => Some(user_text),
            Self::OutputLengthContinuation { .. }
            | Self::StandaloneCompaction { narrative: _ }
            | Self::StandaloneOpaqueCompaction
            | Self::ReactiveOpaqueCompaction { .. }
            | Self::ReactiveCompactedOpaqueText { .. }
            | Self::StandaloneCompactionError {
                failure_kind: _,
                error: _,
            }
            | Self::StandaloneCompactionHold { timeout_ms: _ }
            | Self::TypedImageToolResult { .. }
            | Self::MessageSenderResult { .. }
            | Self::MessageInbound { .. }
            | Self::MessageInboundAfterHeld { .. }
            | Self::ProviderContextRawMessageResult { .. }
            | Self::MessageAndRawInboundAfterHeld { .. }
            | Self::MessageAndRawInboundAfterParallelTools { .. }
            | Self::WatchNotifications { .. }
            | Self::WatchNotificationChains { .. } => None,
        }
    }

    /// Returns whether this closed action admits the provider operation it
    /// explicitly models.
    fn matches_operation(&self, operation: tau_proto::PromptOperation) -> bool {
        match self {
            Self::StandaloneCompaction { narrative: _ }
            | Self::StandaloneOpaqueCompaction
            | Self::ReactiveOpaqueCompaction { .. }
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

/// Validate one unsplit parallel dummy-tool response and complete aggregate.
fn validate_complete_parallel_dummy_round(
    prompt: &tau_proto::AgentPromptCreated,
    expected_call_ids: &[tau_proto::ToolCallId],
) -> Result<(), &'static str> {
    let context = prompt.context.flatten();
    let calls = context
        .iter()
        .filter_map(|item| match item {
            ContextItem::ToolCall(call) => Some(&call.call_id),
            _ => None,
        })
        .collect::<Vec<_>>();
    let results = context
        .iter()
        .filter_map(|item| match item {
            ContextItem::ToolResult(result) => Some(&result.call_id),
            _ => None,
        })
        .collect::<Vec<_>>();
    let expected = expected_call_ids.iter().collect::<Vec<_>>();
    if calls != expected || results != expected {
        return Err("parallel dummy round identity/order mismatch");
    }
    let last_call = context
        .iter()
        .rposition(|item| matches!(item, ContextItem::ToolCall(_)))
        .ok_or("parallel dummy round omitted tool calls")?;
    let first_result = context
        .iter()
        .position(|item| matches!(item, ContextItem::ToolResult(_)))
        .ok_or("parallel dummy round omitted tool results")?;
    let last_result = context
        .iter()
        .rposition(|item| matches!(item, ContextItem::ToolResult(_)))
        .ok_or("parallel dummy round omitted tool results")?;
    if first_result != last_call + 1 || last_result + 1 != first_result + expected_call_ids.len() {
        return Err("parallel dummy round was split");
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

/// Frame fixture-expected harness-owned internal prompt text exactly as
/// production does.
fn tau_internal_envelope(body: &str) -> String {
    let body =
        tau_proto::escape_exact_sentinel_close(body, "</tau_internal>", "&lt;/tau_internal&gt;");
    format!("<tau_internal>{body}</tau_internal>")
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
                        ContentPart::Text { text } | ContentPart::HarnessInternalText { text } => {
                            text
                        }
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect()
}

/// Validate the exact retained transcript consumed by the one authorized
/// output-limit successor.
fn validate_output_length_continuation_context(
    prompt: &tau_proto::AgentPromptCreated,
    user_text: &str,
    reasoning: &str,
) -> Result<(), &'static str> {
    let items = prompt.context.flatten();
    let source_user = items.iter().position(|item| {
        matches!(
            item,
            ContextItem::Message(message)
                if message.role == ContextRole::User
                    && message.content.iter().map(|part| match part {
                        ContentPart::Text { text }
                        | ContentPart::HarnessInternalText { text } => text.as_str(),
                    }).collect::<String>()
                        == project_fixture_human_ui_user_prompt(user_text)
        )
    });
    let retained_reasoning = items.iter().position(|item| {
        matches!(
            item,
            ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
                kind: tau_proto::ReasoningTextKind::Full,
                text,
            }) if text == reasoning
        )
    });
    let instruction_text = tau_internal_envelope(tau_proto::OUTPUT_LENGTH_CONTINUATION_INSTRUCTION);
    let instruction = items.iter().position(|item| {
        matches!(
            item,
            ContextItem::Message(message)
                if message.role == ContextRole::User
                    && message.content.iter().map(|part| match part {
                        ContentPart::Text { text }
                        | ContentPart::HarnessInternalText { text } => text.as_str(),
                    }).collect::<String>() == instruction_text
        )
    });
    let exact_order = matches!(
        (source_user, retained_reasoning, instruction),
        (Some(user), Some(reasoning), Some(instruction))
            if user < reasoning && reasoning < instruction
    );
    let user_texts = provider_user_texts(prompt);
    let exact_reasoning_count = items
        .iter()
        .filter(|item| {
            matches!(
                item,
                ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
                    kind: tau_proto::ReasoningTextKind::Full,
                    text,
                }) if text == reasoning
            )
        })
        .count()
        == 1;
    let no_assistant_or_call = items.iter().all(|item| {
        !matches!(
            item,
            ContextItem::Message(message) if message.role == ContextRole::Assistant
        ) && !matches!(item, ContextItem::ToolCall(_))
    });
    if !exact_order
        || user_texts
            != [
                project_fixture_human_ui_user_prompt(user_text),
                instruction_text,
            ]
        || !exact_reasoning_count
        || !no_assistant_or_call
    {
        return Err("output-length continuation context is not exact ordered replay");
    }
    Ok(())
}

/// Returns whether provider context contains one exact role-specific text item.
fn context_has_text(
    prompt: &tau_proto::AgentPromptCreated,
    role: ContextRole,
    expected: &str,
) -> bool {
    prompt.context.flatten_iter().any(|item| {
        let ContextItem::Message(message) = item else {
            return false;
        };
        if message.role != role {
            return false;
        }
        let actual = message
            .content
            .iter()
            .map(|part| match part {
                ContentPart::Text { text } | ContentPart::HarnessInternalText { text } => {
                    text.as_str()
                }
            })
            .collect::<String>();
        if role == ContextRole::User {
            fixture_user_text_matches(&actual, expected)
        } else {
            actual == expected
        }
    })
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
