//! First-party Zulip long-poll message bridge for Tau agents.
//!
//! The extension uses Zulip's native event queue and HTTP Basic bot
//! authentication. It keeps all native routing authority process-local and
//! emits only canonical external-message reports through
//! `PeerCapability::MessageBridge`.

mod api;
mod checkpoint;
mod config;
mod output;
mod publication_authority;

use std::collections::{HashMap, HashSet, VecDeque};
use std::error::Error;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{Builder as ThreadBuilder, JoinHandle};
use std::time::Duration;

use api::{ApiError, EventQueue, HttpZulipClient, NativeRoute, ZulipClient};
use checkpoint::CheckpointRuntime;
use config::{DirectRoute, ExtConfig, ProactiveRoute, ReceiveMode, RuntimeConfig, StreamRoute};
#[cfg(test)]
pub(crate) use output::MUTATION_PUBLICATION_HOOK;
use output::Output;
#[cfg(test)]
pub(crate) use output::SATURATION_HOOK;
use publication_authority::PublicationAuthority;
use tau_client::{
    ClientResult, ExtensionBuilder, ManualRuntimeInput, ManualRuntimePoll, TauExtension,
};
use tau_proto::{
    AgentId, CborValue, Event, MessageAgentTarget, MessageConversation, MessageDeleted,
    MessageDelivered, MessageEdited, MessageFactId, MessageFactRef, MessageParty,
    MessageReactionAdded, MessageReactionRemoved, MessageSenderAuth, MessageSent,
    RawMessagePublisherId, ToolError, ToolExample, ToolResult, ToolSpec, ToolStarted, ToolUseState,
    ToolUseStatus,
};

/// Tracing target used by this extension.
pub const LOG_TARGET: &str = "zulip";
/// Logical tool name for agent listener registration.
pub const REGISTER_TOOL_NAME: &str = "zulip_register";
/// Logical tool name for configured route discovery.
pub const CONVERSATIONS_TOOL_NAME: &str = "zulip_conversations";
/// Logical tool name for source-bound replies and configured proactive sends.
pub const SEND_TOOL_NAME: &str = "zulip_send";
/// Logical tool name for source-bound emoji reactions.
pub const REACT_TOOL_NAME: &str = "zulip_react";
/// Tool group containing every Zulip bridge tool.
pub const TOOL_GROUP_NAME: &str = "zulip";
/// Policy tag for registration authority.
pub const REGISTER_TOOL_TAG: &str = "zulip:register";
/// Policy tag for route discovery authority.
pub const CONVERSATIONS_TOOL_TAG: &str = "zulip:discover";
/// Policy tag for send authority.
pub const SEND_TOOL_TAG: &str = "zulip:send";
/// Policy tag for reaction authority.
pub const REACT_TOOL_TAG: &str = "zulip:react";

pub(crate) const DEFAULT_MAX_MESSAGE_BYTES: usize = 16 * 1024;
pub(crate) const MAX_MESSAGE_BYTES: usize = 128 * 1024;
pub(crate) const MAX_API_RESPONSE_BYTES: u64 = 256 * 1024;
pub(crate) const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const RECENT_MESSAGE_LIMIT: usize = 4096;
const REPLY_ROUTE_LIMIT: usize = 1024;
const ACTIVE_TOOL_WORKER_LIMIT: usize = 64;
const MAX_DIRECT_NON_BOT_PARTICIPANTS: usize = 32;
const INITIAL_RECONNECT_BACKOFF: Duration = Duration::from_secs(1);
const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(30);

/// Run the Zulip extension over standard input and output.
pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    tau_client::init_logging_for(LOG_TARGET);
    run(std::io::stdin(), std::io::stdout())
}

/// Run the Zulip extension over an arbitrary protocol transport.
pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    run_with_client(reader, writer, Arc::new(HttpZulipClient::default()))
}

/// Final instance-scoped names derived by the SDK.
#[derive(Clone)]
struct ToolNames {
    /// Model-visible namespace label.
    namespace: String,
    /// Registration tool name.
    register: tau_proto::ToolName,
    /// Discovery tool name.
    conversations: tau_proto::ToolName,
    /// Send tool name.
    send: tau_proto::ToolName,
    /// Reaction tool name.
    react: tau_proto::ToolName,
    /// Tool-group name.
    group: tau_proto::ToolGroupName,
}

impl ToolNames {
    fn from_scope(scope: &tau_client::ToolNameScope) -> ClientResult<Self> {
        Ok(Self {
            namespace: scope
                .wire_group_name(&tau_proto::ToolGroupName::new(TOOL_GROUP_NAME))?
                .to_string(),
            register: scope.wire_tool(REGISTER_TOOL_NAME)?,
            conversations: scope.wire_tool(CONVERSATIONS_TOOL_NAME)?,
            send: scope.wire_tool(SEND_TOOL_NAME)?,
            react: scope.wire_tool(REACT_TOOL_NAME)?,
            group: scope.wire_group_name(&tau_proto::ToolGroupName::new(TOOL_GROUP_NAME))?,
        })
    }

    fn logical() -> Self {
        Self {
            namespace: TOOL_GROUP_NAME.to_owned(),
            register: tau_proto::ToolName::new(REGISTER_TOOL_NAME),
            conversations: tau_proto::ToolName::new(CONVERSATIONS_TOOL_NAME),
            send: tau_proto::ToolName::new(SEND_TOOL_NAME),
            react: tau_proto::ToolName::new(REACT_TOOL_NAME),
            group: tau_proto::ToolGroupName::new(TOOL_GROUP_NAME),
        }
    }
}

/// One admitted native conversation retained as private reply authority.
#[derive(Clone, Debug)]
struct Conversation {
    /// Frozen native destination.
    route: NativeRoute,
    /// Stable descriptive fact ID.
    stable_id: String,
    /// Optional configured alias.
    alias: Option<String>,
}

impl Conversation {
    fn fact(&self) -> MessageConversation {
        MessageConversation {
            stable_id: self.stable_id.clone(),
            display_name: None,
            alias: self.alias.clone(),
        }
    }
}

/// Ownership and native authority for one message submitted by this process.
#[derive(Clone)]
struct MessageOwner {
    /// Target Tau agent.
    agent_id: AgentId,
    /// Canonical publisher-scoped fact ID.
    fact_id: MessageFactId,
    /// Native numeric message ID.
    native_message_id: u64,
    /// Frozen source conversation.
    conversation: Conversation,
}

/// Shared mutable lifecycle state.
#[derive(Default)]
struct State {
    /// Current validated configuration.
    config: Option<RuntimeConfig>,
    /// Stable configured publisher name.
    publisher_name: Option<tau_proto::ExtensionName>,
    /// Monotonic configuration generation.
    config_generation: u64,
    /// Monotonic registration generation.
    registration_generation: u64,
    /// Agents currently accepting Zulip messages.
    registered_agents: HashSet<AgentId>,
    /// Agent presentation labels.
    agent_labels: HashMap<AgentId, String>,
    /// Current queue, created before registration succeeds.
    queue: Option<EventQueue>,
    /// Whether the worker thread exists.
    worker_started: bool,
    /// Whether shutdown was requested.
    shutdown_requested: bool,
    /// Network-capable tool workers currently admitted.
    active_tool_workers: usize,
    /// Recent event/message keys used for process-local suppression.
    recent_ids: VecDeque<String>,
    /// Set matching `recent_ids`.
    recent_set: HashSet<String>,
    /// Source-bound routes and mutation ownership indexed by fact ref text.
    owners: HashMap<String, MessageOwner>,
    /// FIFO for bounded owner eviction.
    owner_order: VecDeque<String>,
    /// Durable catch-up state, opened only while the opt-in feature is active.
    checkpoint: Option<CheckpointRuntime>,
}

impl State {
    fn insert_recent(&mut self, key: String) -> bool {
        if self.recent_set.contains(&key) {
            return false;
        }
        self.recent_set.insert(key.clone());
        self.recent_ids.push_back(key);
        while RECENT_MESSAGE_LIMIT < self.recent_ids.len() {
            if let Some(old) = self.recent_ids.pop_front() {
                self.recent_set.remove(&old);
            }
        }
        true
    }

    fn insert_owner(&mut self, owner: MessageOwner) {
        let key = owner.fact_id.as_str().to_owned();
        if !self.owners.contains_key(&key) {
            self.owner_order.push_back(key.clone());
        }
        self.owners.insert(key, owner);
        while REPLY_ROUTE_LIMIT < self.owner_order.len() {
            if let Some(old) = self.owner_order.pop_front() {
                self.owners.remove(&old);
            }
        }
    }

    fn clear_authority(&mut self) {
        self.queue = None;
        self.registered_agents.clear();
        self.owners.clear();
        self.owner_order.clear();
        self.recent_ids.clear();
        self.recent_set.clear();
        self.registration_generation = self.registration_generation.wrapping_add(1);
        self.checkpoint = None;
    }

    fn unregister_agent(&mut self, agent_id: &AgentId) {
        self.registered_agents.remove(agent_id);
        self.owners.retain(|_, owner| &owner.agent_id != agent_id);
        if self.registered_agents.is_empty() {
            self.queue = None;
            self.checkpoint = None;
        }
    }
}

/// Mutex and condition variable coordinating tools with the poll worker.
struct SharedState {
    /// Mutable state.
    state: Mutex<State>,
    /// Worker wakeup on registration, config, or shutdown changes.
    changed: Condvar,
}

/// RAII reservation that bounds network-capable tool worker threads.
struct ToolWorkerPermit {
    /// Shared state containing the active-worker count.
    state: Arc<SharedState>,
}

impl Drop for ToolWorkerPermit {
    fn drop(&mut self) {
        let mut state = self.state.lock();
        state.active_tool_workers = state.active_tool_workers.saturating_sub(1);
    }
}

impl SharedState {
    fn new() -> Self {
        Self {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }
}

/// Zulip bridge runtime and worker dependencies.
struct Extension {
    /// Shared lifecycle state.
    state: Arc<SharedState>,
    /// HTTP API implementation.
    client: Arc<dyn ZulipClient>,
    /// Serialized protocol output.
    output: Output,
    /// Fast shutdown marker.
    shutdown: Arc<AtomicBool>,
    /// Instance-scoped tools.
    tool_names: ToolNames,
    /// Serializes authority retirement against canonical report publication.
    publication_authority: Arc<PublicationAuthority>,
    /// Join handle used to settle queue-worker cleanup before runner exit.
    worker_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl Extension {
    fn new(client: Arc<dyn ZulipClient>, output: impl Into<Output>, tool_names: ToolNames) -> Self {
        Self {
            state: Arc::new(SharedState::new()),
            client,
            output: output.into(),
            shutdown: Arc::new(AtomicBool::new(false)),
            tool_names,
            publication_authority: Arc::new(PublicationAuthority::new()),
            worker_handle: Arc::new(Mutex::new(None)),
        }
    }

    fn apply_config(&self, cfg: RuntimeConfig, publisher: tau_proto::ExtensionName) {
        let _authority = self.publication_authority.retire();
        let mut state = self.state.lock();
        state.config_generation = state.config_generation.wrapping_add(1);
        state.clear_authority();
        state.config = Some(cfg);
        state.publisher_name = Some(publisher);
        self.state.changed.notify_all();
    }

    fn clear_config(&self) {
        let _authority = self.publication_authority.retire();
        let mut state = self.state.lock();
        state.config_generation = state.config_generation.wrapping_add(1);
        state.clear_authority();
        state.config = None;
        self.state.changed.notify_all();
    }

    fn handles_tool(&self, name: &str) -> bool {
        [
            self.tool_names.register.as_str(),
            self.tool_names.conversations.as_str(),
            self.tool_names.send.as_str(),
            self.tool_names.react.as_str(),
        ]
        .contains(&name)
    }

    fn dispatch_tool_checked(&self, invoke: ToolStarted) -> ClientResult<()> {
        self.output.progress(&invoke);
        if invoke.tool_name.as_str() == self.tool_names.conversations.as_str() {
            let event = self.handle_conversations(invoke);
            return self.output.terminal(event);
        }
        let permit = {
            let mut state = self.state.lock();
            if ACTIVE_TOOL_WORKER_LIMIT <= state.active_tool_workers {
                drop(state);
                return self.output.terminal(tool_error(
                    invoke,
                    "zulip network tool concurrency limit reached".to_owned(),
                ));
            }
            state.active_tool_workers += 1;
            ToolWorkerPermit {
                state: Arc::clone(&self.state),
            }
        };
        let registration_epoch = (invoke.tool_name.as_str() == self.tool_names.register.as_str())
            .then(|| {
                validate_fields(&invoke.arguments, &["enabled"])
                    .and_then(|_| bool_field(&invoke.arguments, "enabled"))
            })
            .transpose()
            .ok()
            .flatten()
            .map(|_enabled| {
                let _authority = self.publication_authority.retire();
                let mut state = self.state.lock();
                state.registration_generation = state.registration_generation.wrapping_add(1);
                state.unregister_agent(&invoke.agent_id);
                let epoch = state.registration_generation;
                self.state.changed.notify_all();
                epoch
            });
        let ext = self.clone_for_worker();
        let spawn_failure = invoke.clone();
        let output = self.output.clone();
        let spawn_result = ThreadBuilder::new()
            .name("tau-zulip-tool".to_owned())
            .spawn(move || {
                let _permit = permit;
                let event = if invoke.tool_name.as_str() == ext.tool_names.register.as_str() {
                    ext.handle_register(invoke, registration_epoch)
                } else if invoke.tool_name.as_str() == ext.tool_names.send.as_str() {
                    ext.handle_send(invoke)
                } else if invoke.tool_name.as_str() == ext.tool_names.react.as_str() {
                    ext.handle_react(invoke)
                } else {
                    tool_error(invoke, "unknown zulip tool".to_owned())
                };
                if ext.output.check_mandatory_output().is_ok() {
                    let _ = ext.output.terminal(event);
                }
            });
        if spawn_result.is_err() {
            return output.terminal(tool_error(
                spawn_failure,
                "zulip network tool worker could not start".to_owned(),
            ));
        }
        Ok(())
    }

    /// Dispatch one tool for direct unit tests whose channel remains connected.
    #[cfg(test)]
    fn dispatch_tool(&self, invoke: ToolStarted) {
        self.dispatch_tool_checked(invoke)
            .expect("test output channel remains connected");
    }

    fn clone_for_worker(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
            client: Arc::clone(&self.client),
            output: self.output.clone(),
            shutdown: Arc::clone(&self.shutdown),
            tool_names: self.tool_names.clone(),
            publication_authority: Arc::clone(&self.publication_authority),
            worker_handle: Arc::clone(&self.worker_handle),
        }
    }

    /// Resolve configured channel names, establish required subscriptions, and
    /// create a queue whose routes use only authenticated native IDs.
    fn acquire_queue(&self, cfg: &RuntimeConfig) -> Result<(RuntimeConfig, EventQueue), ApiError> {
        let mut resolved = cfg.clone();
        resolved.routes = cfg
            .configured_routes
            .iter()
            .map(|route| {
                self.client
                    .resolve_stream_id(cfg, &route.name)
                    .map(|stream_id| route.resolve(stream_id))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if resolved.routes.iter().enumerate().any(|(index, route)| {
            resolved.routes[index + 1..].iter().any(|other| {
                route.stream_id == other.stream_id
                    && route.receive.is_some()
                    && other.receive.is_some()
                    && (route.topic.is_none()
                        || other.topic.is_none()
                        || route.topic == other.topic)
            })
        }) {
            return Err(ApiError::invalid_request());
        }
        let all_messages = resolved
            .routes
            .iter()
            .filter(|route| route.receive == Some(ReceiveMode::AllMessages))
            .map(|route| route.name.clone())
            .collect::<Vec<_>>();
        if !all_messages.is_empty() {
            self.client.subscribe(&resolved, &all_messages)?;
        }
        let queue = self.client.register_queue(&resolved)?;
        Ok((resolved, queue))
    }

    fn handle_register(&self, invoke: ToolStarted, registration_epoch: Option<u64>) -> Event {
        if let Err(error) = validate_fields(&invoke.arguments, &["enabled"]) {
            return tool_error(invoke, error);
        }
        let enabled = match bool_field(&invoke.arguments, "enabled") {
            Ok(value) => value,
            Err(error) => return tool_error(invoke, error),
        };
        let Some(registration_epoch) = registration_epoch else {
            return tool_error(invoke, "invalid Zulip registration intent".to_owned());
        };
        if !enabled {
            let state = self.state.lock();
            if state.registration_generation != registration_epoch {
                return tool_error(
                    invoke,
                    "zulip registration intent was superseded".to_owned(),
                );
            }
            self.state.changed.notify_all();
            return tool_result(
                invoke,
                serde_json::json!({"status":"unregistered"}).to_string(),
            );
        }
        let (cfg, generation, registration_generation, needs_queue) = {
            let state = self.state.lock();
            if state.registration_generation != registration_epoch {
                return tool_error(
                    invoke,
                    "zulip registration intent was superseded".to_owned(),
                );
            }
            let Some(cfg) = state.config.clone() else {
                return tool_error(invoke, "zulip extension is not configured".to_owned());
            };
            (
                cfg,
                state.config_generation,
                registration_epoch,
                state.queue.is_none(),
            )
        };
        let prepared = if needs_queue {
            match self.acquire_queue(&cfg) {
                Ok(value) => Some(value),
                Err(error) => return tool_error(invoke, error.diagnostic()),
            }
        } else {
            None
        };
        let checkpoint = if needs_queue && cfg.offline_message_catch_up {
            let Some(state_dir) = cfg.state_dir.as_deref() else {
                return tool_error(
                    invoke,
                    "offline Zulip catch-up requires harness extension state_dir".to_owned(),
                );
            };
            match CheckpointRuntime::open(state_dir, &cfg.id_key) {
                Ok(checkpoint) => Some(checkpoint),
                Err(_) => {
                    return tool_error(
                        invoke,
                        "Zulip message checkpoint is unavailable or already in use".to_owned(),
                    );
                }
            }
        } else {
            None
        };
        let _authority = self.publication_authority.retire();
        let mut state = self.state.lock();
        if state.config_generation != generation
            || state.registration_generation != registration_generation
            || state.config.is_none()
        {
            return tool_error(
                invoke,
                "zulip configuration changed during registration".to_owned(),
            );
        }
        if let Some((cfg, queue)) = prepared {
            state.config = Some(cfg);
            state.queue = Some(queue);
        }
        if let Some(checkpoint) = checkpoint {
            state.checkpoint = Some(checkpoint);
        }
        state.registered_agents.insert(invoke.agent_id.clone());
        state
            .agent_labels
            .entry(invoke.agent_id.clone())
            .or_insert_with(|| invoke.agent_id.to_string());
        if !self.ensure_worker(&mut state) {
            state.registered_agents.remove(&invoke.agent_id);
            state.registration_generation = state.registration_generation.wrapping_add(1);
            return tool_error(invoke, "zulip event worker could not start".to_owned());
        }
        self.state.changed.notify_all();
        tool_result(invoke, serde_json::json!({"status":"registered","incoming_transport_reference":"@zulip_bridge"}).to_string())
    }

    fn ensure_worker(&self, state: &mut State) -> bool {
        if state.worker_started {
            return true;
        }
        state.worker_started = true;
        let worker = Arc::new(self.clone_for_worker());
        let mut worker_handle = self
            .worker_handle
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let handle = match ThreadBuilder::new()
            .name("tau-zulip-events".to_owned())
            .spawn(move || worker_loop(worker))
        {
            Ok(handle) => handle,
            Err(_) => {
                state.worker_started = false;
                return false;
            }
        };
        *worker_handle = Some(handle);
        true
    }

    fn handle_conversations(&self, invoke: ToolStarted) -> Event {
        if let Err(error) = validate_fields(&invoke.arguments, &[]) {
            return tool_error(invoke, error);
        }
        let state = self.state.lock();
        if !state.registered_agents.contains(&invoke.agent_id) {
            return tool_error(
                invoke,
                format!(
                    "{} requires {}(enabled: true) first",
                    self.tool_names.conversations, self.tool_names.register
                ),
            );
        }
        let Some(cfg) = state.config.as_ref() else {
            return tool_error(invoke, "zulip extension is not configured".to_owned());
        };
        let routes = cfg
            .routes
            .iter()
            .filter(|route| route.proactive.is_enabled())
            .map(|route| {
                serde_json::json!({
                    "alias": route.name,
                    "kind": if route.proactive.allows_agent_chosen_topic() {
                        "stream"
                    } else {
                        "stream_topic"
                    },
                    "topic": route.topic,
                    "agent_chosen_topic": route.proactive.allows_agent_chosen_topic(),
                    "description": route.description,
                })
            })
            .chain(cfg.direct_routes.iter().map(|route| {
                serde_json::json!({
                    "alias": route.alias(),
                    "kind": "direct",
                    "topic": serde_json::Value::Null,
                    "agent_chosen_topic": false,
                    "description": route.description(),
                })
            }))
            .collect::<Vec<_>>();
        tool_result(
            invoke,
            serde_json::json!({"conversations": routes}).to_string(),
        )
    }

    fn handle_send(&self, invoke: ToolStarted) -> Event {
        if let Err(error) = validate_fields(
            &invoke.arguments,
            &["message", "reply_to", "destination", "topic"],
        ) {
            return tool_error(invoke, error);
        }
        let message = match string_field(&invoke.arguments, "message") {
            Ok(value) => value,
            Err(error) => return tool_error(invoke, error),
        };
        let reply_to = match optional_string_field(&invoke.arguments, "reply_to") {
            Ok(value) => value,
            Err(error) => return tool_error(invoke, error),
        };
        let destination = match optional_string_field(&invoke.arguments, "destination") {
            Ok(value) => value,
            Err(error) => return tool_error(invoke, error),
        };
        let requested_topic = match optional_string_field(&invoke.arguments, "topic") {
            Ok(value) => value,
            Err(error) => return tool_error(invoke, error),
        };
        if message.trim().is_empty() {
            return tool_error(invoke, "`message` must not be empty".to_owned());
        }
        if reply_to.is_some() == destination.is_some() {
            return tool_error(
                invoke,
                "provide exactly one of `reply_to` or `destination`".to_owned(),
            );
        }
        let (cfg, route, conversation, generation, registration_generation) = {
            let state = self.state.lock();
            if !state.registered_agents.contains(&invoke.agent_id) {
                return tool_error(
                    invoke,
                    format!(
                        "{} requires {}(enabled: true) first",
                        self.tool_names.send, self.tool_names.register
                    ),
                );
            }
            let Some(cfg) = state.config.clone() else {
                return tool_error(invoke, "zulip extension is not configured".to_owned());
            };
            if cfg.max_message_bytes < message.len() {
                return tool_error(
                    invoke,
                    "zulip message exceeds configured byte limit".to_owned(),
                );
            }
            let conversation = if let Some(reference) = reply_to.as_ref() {
                let Some(owner) = state
                    .owners
                    .get(reference)
                    .filter(|owner| owner.agent_id == invoke.agent_id)
                else {
                    return tool_error(
                        invoke,
                        "unknown, stale, or unauthorized Zulip reply reference".to_owned(),
                    );
                };
                owner.conversation.clone()
            } else {
                let alias = destination.as_deref().unwrap_or_default();
                if let Some(route) = cfg
                    .direct_routes
                    .iter()
                    .find(|route| route.alias() == alias)
                {
                    if requested_topic.is_some() {
                        return tool_error(
                            invoke,
                            "`topic` is allowed only for destinations with \
                             `agent_chosen_topic` authority"
                                .to_owned(),
                        );
                    }
                    direct_conversation(&cfg, route)
                } else {
                    let Some(route) = cfg.routes.iter().find(|route| route.name == alias) else {
                        return tool_error(
                            invoke,
                            "unknown or unauthorized Zulip destination alias".to_owned(),
                        );
                    };
                    let topic = match &route.proactive {
                        ProactiveRoute::Disabled => {
                            return tool_error(
                                invoke,
                                "unknown or unauthorized Zulip destination alias".to_owned(),
                            );
                        }
                        ProactiveRoute::AgentChosenTopic => {
                            let Some(topic) = requested_topic.as_deref() else {
                                return tool_error(
                                    invoke,
                                    "`topic` is required for this Zulip destination".to_owned(),
                                );
                            };
                            if let Err(error) = validate_agent_topic(topic) {
                                return tool_error(invoke, error);
                            }
                            topic
                        }
                        ProactiveRoute::ExactTopic(topic) => {
                            if requested_topic.is_some() {
                                return tool_error(
                                    invoke,
                                    "`topic` is allowed only for destinations with \
                                     `agent_chosen_topic` authority"
                                        .to_owned(),
                                );
                            }
                            topic
                        }
                    };
                    stream_conversation(&cfg, route, topic)
                }
            };
            if reply_to.is_some() && requested_topic.is_some() {
                return tool_error(
                    invoke,
                    "`topic` cannot be used with a Zulip reply reference".to_owned(),
                );
            }
            (
                cfg,
                conversation.route.clone(),
                conversation,
                state.config_generation,
                state.registration_generation,
            )
        };
        let sent = match self.client.send_message(&cfg, &route, &message) {
            Ok(value) => value,
            Err(error) => return tool_error(invoke, error.diagnostic()),
        };
        let message_ref = message_fact_id(&cfg, sent.message_id);
        let _authority = self.publication_authority.publish();
        let publisher = {
            let state = self.state.lock();
            if state.config_generation != generation
                || state.registration_generation != registration_generation
                || !state.registered_agents.contains(&invoke.agent_id)
            {
                return tool_error(
                    invoke,
                    "zulip authority changed while sending; remote outcome may have succeeded"
                        .to_owned(),
                );
            }
            state.publisher_name.clone().expect("configured publisher")
        };
        let report = Event::MessageSentReported(MessageSent::new(
            RawMessagePublisherId::new(publisher.as_str()),
            MessageAgentTarget::new(invoke.agent_id.as_ref()),
            message_ref.clone(),
            None,
            Some(conversation.fact()),
            message.clone(),
        ));
        if !self.output.emit_message_report(report) {
            return tool_error(
                invoke,
                "zulip local sent-report submission failed after remote success".to_owned(),
            );
        }
        {
            let mut state = self.state.lock();
            if state.config_generation != generation
                || state.registration_generation != registration_generation
                || !state.registered_agents.contains(&invoke.agent_id)
            {
                return tool_error(
                    invoke,
                    "zulip authority changed after sent-report publication".to_owned(),
                );
            }
            state.insert_owner(MessageOwner {
                agent_id: invoke.agent_id.clone(),
                fact_id: message_ref.clone(),
                native_message_id: sent.message_id,
                conversation,
            });
        }
        tool_result(invoke, serde_json::json!({"status":"sent","message_ref":message_ref.as_str(),"delivery_copies":"one"}).to_string())
    }

    fn handle_react(&self, invoke: ToolStarted) -> Event {
        if let Err(error) = validate_fields(&invoke.arguments, &["message_ref", "emoji", "action"])
        {
            return tool_error(invoke, error);
        }
        let reference = match string_field(&invoke.arguments, "message_ref") {
            Ok(value) => value,
            Err(error) => return tool_error(invoke, error),
        };
        let emoji = match string_field(&invoke.arguments, "emoji") {
            Ok(value) => value,
            Err(error) => return tool_error(invoke, error),
        };
        let action = match string_field(&invoke.arguments, "action") {
            Ok(value) => value,
            Err(error) => return tool_error(invoke, error),
        };
        if emoji.is_empty()
            || 64 < emoji.len()
            || !emoji.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
            })
        {
            return tool_error(
                invoke,
                "`emoji` must be a bounded Zulip emoji name".to_owned(),
            );
        }
        let add = match action.as_str() {
            "add" => true,
            "remove" => false,
            _ => return tool_error(invoke, "`action` must be `add` or `remove`".to_owned()),
        };
        let (cfg, native_id, generation, registration_generation) = {
            let state = self.state.lock();
            if !state.registered_agents.contains(&invoke.agent_id) {
                return tool_error(
                    invoke,
                    format!(
                        "{} requires {}(enabled: true) first",
                        self.tool_names.react, self.tool_names.register
                    ),
                );
            }
            let Some(owner) = state
                .owners
                .get(&reference)
                .filter(|owner| owner.agent_id == invoke.agent_id)
            else {
                return tool_error(
                    invoke,
                    "unknown, stale, or unauthorized Zulip message reference".to_owned(),
                );
            };
            (
                state.config.clone().expect("registered config"),
                owner.native_message_id,
                state.config_generation,
                state.registration_generation,
            )
        };
        if let Err(error) = self.client.react(&cfg, native_id, &emoji, add) {
            return tool_error(invoke, error.diagnostic());
        }
        let state = self.state.lock();
        if state.config_generation != generation
            || state.registration_generation != registration_generation
            || !state.registered_agents.contains(&invoke.agent_id)
        {
            return tool_error(
                invoke,
                "zulip authority changed while reacting; remote outcome may have succeeded"
                    .to_owned(),
            );
        }
        tool_result(
            invoke,
            serde_json::json!({"status": if add {"added"} else {"removed"}}).to_string(),
        )
    }

    fn process_event(
        &self,
        event: serde_json::Value,
        generation: u64,
        registration_generation: u64,
        bot_user_id: u64,
    ) {
        let _authority = self.publication_authority.publish();
        self.process_event_guarded(event, generation, registration_generation, bot_user_id);
    }

    /// Process an event while the caller owns publication authority.
    fn process_event_guarded(
        &self,
        event: serde_json::Value,
        generation: u64,
        registration_generation: u64,
        bot_user_id: u64,
    ) {
        let event_id = match event.get("id").and_then(serde_json::Value::as_i64) {
            Some(value) => value,
            None => return,
        };
        let kind = event
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if kind == "heartbeat" {
            return;
        }
        match kind {
            "message" => {
                self.process_message(&event, generation, registration_generation, bot_user_id)
            }
            "update_message" => self.process_mutation(
                event_id,
                &event,
                generation,
                registration_generation,
                Mutation::Edit,
            ),
            "delete_message" => self.process_mutation(
                event_id,
                &event,
                generation,
                registration_generation,
                Mutation::Delete,
            ),
            "reaction" => self.process_mutation(
                event_id,
                &event,
                generation,
                registration_generation,
                Mutation::Reaction,
            ),
            _ => {}
        }
    }

    fn process_message(
        &self,
        event: &serde_json::Value,
        generation: u64,
        registration_generation: u64,
        bot_user_id: u64,
    ) {
        let message = event.get("message").unwrap_or(event);
        let Some(native_id) = message.get("id").and_then(serde_json::Value::as_u64) else {
            return;
        };
        let Some(sender_id) = message.get("sender_id").and_then(serde_json::Value::as_u64) else {
            return;
        };
        let Some(text) = message.get("content").and_then(serde_json::Value::as_str) else {
            return;
        };
        let mentioned = event
            .get("flags")
            .or_else(|| message.get("flags"))
            .and_then(serde_json::Value::as_array)
            .is_some_and(|flags| flags.iter().any(|flag| flag.as_str() == Some("mentioned")));
        if sender_id == bot_user_id {
            return;
        }
        let (agent_id, conversation, sender_alias) = {
            let mut state = self.state.lock();
            if state.config_generation != generation
                || state.registration_generation != registration_generation
                || state.queue.is_none()
                || state.registered_agents.len() != 1
            {
                return;
            }
            if !state.insert_recent(format!("message:{native_id}")) {
                return;
            }
            let cfg = state.config.clone().expect("active queue config");
            if !cfg.allowed_user_ids.contains(&sender_id) || cfg.max_message_bytes < text.len() {
                return;
            }
            let Some(conversation) =
                admitted_conversation(&cfg, message, sender_id, bot_user_id, mentioned)
            else {
                return;
            };
            let agent_id = state
                .registered_agents
                .iter()
                .next()
                .cloned()
                .expect("one agent");
            let sender_alias = cfg.sender_aliases.get(&sender_id).cloned();
            (agent_id, conversation, sender_alias)
        };
        let mut state = self.state.lock();
        if state.config_generation != generation
            || state.registration_generation != registration_generation
            || state.queue.is_none()
            || !state.registered_agents.contains(&agent_id)
        {
            return;
        }
        let cfg = state.config.as_ref().expect("active queue config");
        let fact_id = message_fact_id(cfg, native_id);
        let publisher = state.publisher_name.clone().expect("configured publisher");
        let report = Event::MessageDeliveredReported(MessageDelivered::new(
            RawMessagePublisherId::new(publisher.as_str()),
            MessageAgentTarget::new(agent_id.as_ref()),
            fact_id.clone(),
            sender_party(cfg, sender_id, sender_alias),
            Some(conversation.fact()),
            text,
        ));
        if let Some(checkpoint) = state.checkpoint.as_mut() {
            checkpoint.submitted(native_id, fact_id.clone());
        }
        state.insert_owner(MessageOwner {
            agent_id: agent_id.clone(),
            fact_id: fact_id.clone(),
            native_message_id: native_id,
            conversation: conversation.clone(),
        });
        drop(state);
        {
            let state = self.state.lock();
            if state.config_generation != generation
                || state.registration_generation != registration_generation
                || !state.registered_agents.contains(&agent_id)
            {
                return;
            }
        }
        if !self.output.emit_message_report(report) {
            let mut state = self.state.lock();
            if state.config_generation != generation
                || state.registration_generation != registration_generation
                || !state.registered_agents.contains(&agent_id)
            {
                return;
            }
            if let Some(checkpoint) = state.checkpoint.as_mut() {
                checkpoint.retry(native_id);
            }
            if state
                .owners
                .get(fact_id.as_str())
                .is_some_and(|owner| owner.native_message_id == native_id)
            {
                state.owners.remove(fact_id.as_str());
            }
            state.recent_set.remove(&format!("message:{native_id}"));
            state
                .recent_ids
                .retain(|key| key != &format!("message:{native_id}"));
        }
    }

    fn observe_created_message(
        &self,
        event: serde_json::Value,
        generation: u64,
        registration_generation: u64,
        bot_user_id: u64,
    ) {
        let _authority = self.publication_authority.publish();
        let message = event.get("message").unwrap_or(&event);
        let Some(native_id) = message.get("id").and_then(serde_json::Value::as_u64) else {
            self.process_event_guarded(event, generation, registration_generation, bot_user_id);
            return;
        };
        {
            let mut state = self.state.lock();
            if state.config_generation != generation
                || state.registration_generation != registration_generation
                || state
                    .queue
                    .as_ref()
                    .is_none_or(|queue| queue.bot_user_id != bot_user_id)
            {
                return;
            }
            let Some(checkpoint) = state.checkpoint.as_mut() else {
                drop(state);
                self.process_event_guarded(event, generation, registration_generation, bot_user_id);
                return;
            };
            if !checkpoint.begin(native_id) {
                return;
            }
        }
        self.process_event_guarded(event, generation, registration_generation, bot_user_id);
        let mut state = self.state.lock();
        let fact_id = state
            .owners
            .values()
            .find(|owner| owner.native_message_id == native_id)
            .map(|owner| owner.fact_id.clone());
        let was_processed = state.recent_set.contains(&format!("message:{native_id}"));
        if let Some(checkpoint) = state.checkpoint.as_mut() {
            if let Some(fact_id) = fact_id {
                checkpoint.submitted(native_id, fact_id);
            } else if was_processed {
                checkpoint.filtered(native_id);
            } else {
                checkpoint.retry(native_id);
            }
        }
        if let Some(checkpoint) = state.checkpoint.as_mut()
            && let Err(error) = checkpoint.advance()
        {
            tracing::warn!(target: LOG_TARGET, category = %error, "Zulip checkpoint write failed");
        }
    }

    fn process_mutation(
        &self,
        event_id: i64,
        event: &serde_json::Value,
        generation: u64,
        registration_generation: u64,
        mutation: Mutation,
    ) {
        let native_id = event
            .get("message_id")
            .and_then(serde_json::Value::as_u64)
            .or_else(|| {
                event
                    .get("message")
                    .and_then(|value| value.get("id"))
                    .and_then(serde_json::Value::as_u64)
            });
        let Some(native_id) = native_id else {
            return;
        };
        let actor_id = event
            .get("user_id")
            .and_then(serde_json::Value::as_u64)
            .or_else(|| {
                event
                    .get("message")
                    .and_then(|value| value.get("sender_id"))
                    .and_then(serde_json::Value::as_u64)
            });
        let (owner, publisher, actor) = {
            let mut state = self.state.lock();
            if state.config_generation != generation
                || state.registration_generation != registration_generation
                || !state.insert_recent(format!("mutation:{event_id}"))
            {
                return;
            }
            let Some(owner) = state
                .owners
                .values()
                .find(|owner| owner.native_message_id == native_id)
                .cloned()
            else {
                return;
            };
            let cfg = state.config.as_ref().expect("active config");
            let actor = actor_id
                .filter(|id| cfg.allowed_user_ids.contains(id))
                .map(|id| sender_party(cfg, id, cfg.sender_aliases.get(&id).cloned()));
            if actor_id.is_some() && actor.is_none() {
                return;
            }
            (
                owner,
                state.publisher_name.clone().expect("publisher"),
                actor,
            )
        };
        let publisher = RawMessagePublisherId::new(publisher.as_str());
        let target = MessageFactRef {
            publisher_extension_id: publisher.clone(),
            message_id: owner.fact_id.clone(),
        };
        let agent = MessageAgentTarget::new(owner.agent_id.as_ref());
        let conversation = Some(owner.conversation.fact());
        let report = match mutation {
            Mutation::Edit => {
                let Some(text) = event
                    .get("message")
                    .and_then(|value| value.get("content"))
                    .or_else(|| event.get("content"))
                    .and_then(serde_json::Value::as_str)
                else {
                    return;
                };
                Event::MessageEditedReported(MessageEdited::new(
                    publisher,
                    agent,
                    target,
                    actor,
                    conversation,
                    text,
                ))
            }
            Mutation::Delete => Event::MessageDeletedReported(MessageDeleted::new(
                publisher,
                agent,
                target,
                actor,
                conversation,
            )),
            Mutation::Reaction => {
                let Some(emoji) = event
                    .get("emoji_name")
                    .and_then(serde_json::Value::as_str)
                    .filter(|value| value.len() <= 64)
                else {
                    return;
                };
                match event.get("op").and_then(serde_json::Value::as_str) {
                    Some("add") => Event::MessageReactionAddedReported(MessageReactionAdded::new(
                        publisher,
                        agent,
                        target,
                        actor,
                        conversation,
                        emoji,
                    )),
                    Some("remove") => {
                        Event::MessageReactionRemovedReported(MessageReactionRemoved::new(
                            publisher,
                            agent,
                            target,
                            actor,
                            conversation,
                            emoji,
                        ))
                    }
                    _ => return,
                }
            }
        };
        let mut state = self.state.lock();
        let owner_is_current = state
            .owners
            .get(owner.fact_id.as_str())
            .is_some_and(|current| {
                current.native_message_id == owner.native_message_id
                    && current.agent_id == owner.agent_id
            });
        let edit_is_too_large = matches!(
            &report,
            Event::MessageEditedReported(edit)
                if state
                    .config
                    .as_ref()
                    .is_none_or(|cfg| cfg.max_message_bytes < edit.text.len())
        );
        if state.config_generation != generation
            || state.registration_generation != registration_generation
            || !state.registered_agents.contains(&owner.agent_id)
            || !owner_is_current
            || edit_is_too_large
        {
            return;
        }
        if self.output.emit_message_report(report)
            && matches!(mutation, Mutation::Delete)
            && state
                .owners
                .get(owner.fact_id.as_str())
                .is_some_and(|current| {
                    current.native_message_id == owner.native_message_id
                        && current.agent_id == owner.agent_id
                })
        {
            state.owners.remove(owner.fact_id.as_str());
        }
    }

    fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _authority = self.publication_authority.retire();
        let mut state = self.state.lock();
        state.shutdown_requested = true;
        state.clear_authority();
        self.state.changed.notify_all();
        drop(state);
        drop(_authority);
        if let Some(handle) = self
            .worker_handle
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            let _ = handle.join();
        }
    }

    /// Retire authority promptly on normal harness disconnect without waiting
    /// for the provider's already-running long poll.
    fn request_shutdown_detached(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _authority = self.publication_authority.retire();
        let mut state = self.state.lock();
        state.shutdown_requested = true;
        state.clear_authority();
        self.state.changed.notify_all();
    }

    fn catch_up_messages(
        &self,
        cfg: &RuntimeConfig,
        queue: &EventQueue,
        generation: u64,
        registration_generation: u64,
    ) -> Result<(), ApiError> {
        const PAGE_SIZE: usize = 100;
        let state_is_current = || {
            let state = self.state.lock();
            state.config_generation == generation
                && state.registration_generation == registration_generation
                && state
                    .queue
                    .as_ref()
                    .is_some_and(|current| current.queue_id == queue.queue_id)
        };

        let initial_position = self
            .state
            .lock()
            .checkpoint
            .as_ref()
            .and_then(CheckpointRuntime::position);
        let retry_position = self
            .state
            .lock()
            .checkpoint
            .as_ref()
            .and_then(CheckpointRuntime::retry_position);
        if let Some(retry_position) = retry_position {
            let page = self
                .client
                .get_messages_after(cfg, retry_position.saturating_sub(1), 1)?;
            if !state_is_current() {
                return Ok(());
            }
            let Some(message) = page.messages.into_iter().find(|message| {
                message.get("id").and_then(serde_json::Value::as_u64) == Some(retry_position)
            }) else {
                return Err(ApiError::MalformedResponse);
            };
            self.observe_created_message(
                serde_json::json!({"id": retry_position, "type": "message", "message": message}),
                generation,
                registration_generation,
                queue.bot_user_id,
            );
            return Ok(());
        }
        let exhausted;
        if let Some(mut after) = initial_position {
            if self
                .state
                .lock()
                .checkpoint
                .as_ref()
                .is_some_and(CheckpointRuntime::has_outstanding)
            {
                return Ok(());
            }
            let page = self.client.get_messages_after(cfg, after, PAGE_SIZE)?;
            if !state_is_current() {
                return Ok(());
            }
            if page.messages.is_empty() && !page.found_newest {
                return Err(ApiError::MalformedResponse);
            }
            for message in page.messages {
                let native_id = message
                    .get("id")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or(ApiError::MalformedResponse)?;
                if native_id <= after {
                    return Err(ApiError::MalformedResponse);
                }
                after = native_id;
                self.observe_created_message(
                    serde_json::json!({"id": native_id, "type": "message", "message": message}),
                    generation,
                    registration_generation,
                    queue.bot_user_id,
                );
            }
            exhausted = page.found_newest;
        } else {
            exhausted = true;
            let baseline = self.client.newest_message_id(cfg)?.unwrap_or(0);
            if !state_is_current() {
                return Ok(());
            }
            let events = self
                .client
                .get_events_now(cfg, &queue.queue_id, queue.last_event_id)?;
            if !state_is_current() {
                return Ok(());
            }
            let mut last_event_id = queue.last_event_id;
            for event in events {
                let event_id = event.get("id").and_then(serde_json::Value::as_i64);
                if event.get("type").and_then(serde_json::Value::as_str) == Some("message") {
                    self.observe_created_message(
                        event,
                        generation,
                        registration_generation,
                        queue.bot_user_id,
                    );
                }
                if self.output.mandatory_output_failed() {
                    break;
                }
                if let Some(event_id) = event_id {
                    last_event_id = last_event_id.max(event_id);
                }
            }
            let mut state = self.state.lock();
            if state.config_generation != generation
                || state.registration_generation != registration_generation
                || state
                    .queue
                    .as_ref()
                    .is_none_or(|current| current.queue_id != queue.queue_id)
            {
                return Ok(());
            }
            if let Some(checkpoint) = state.checkpoint.as_mut() {
                checkpoint.baseline(baseline);
            }
            if let Some(current) = state
                .queue
                .as_mut()
                .filter(|current| current.queue_id == queue.queue_id)
            {
                current.last_event_id = last_event_id;
            }
            if let Some(checkpoint) = state.checkpoint.as_mut()
                && let Err(error) = checkpoint.advance()
            {
                tracing::warn!(target: LOG_TARGET, category = %error, "Zulip checkpoint write failed");
            }
        }
        let mut state = self.state.lock();
        if state.config_generation != generation
            || state.registration_generation != registration_generation
            || state
                .queue
                .as_ref()
                .is_none_or(|current| current.queue_id != queue.queue_id)
        {
            return Ok(());
        }
        if let Some(checkpoint) = state.checkpoint.as_mut() {
            checkpoint.set_more_history(!exhausted);
        }
        Ok(())
    }

    fn wait_for_checkpoint_progress(&self, generation: u64, registration_generation: u64) {
        let state = self.state.lock();
        if state
            .checkpoint
            .as_ref()
            .is_some_and(CheckpointRuntime::has_outstanding)
        {
            drop(
                self.state
                    .changed
                    .wait_while(state, |state| {
                        !state.shutdown_requested
                            && state.config_generation == generation
                            && state.registration_generation == registration_generation
                            && state
                                .checkpoint
                                .as_ref()
                                .is_some_and(CheckpointRuntime::has_outstanding)
                    })
                    .unwrap_or_else(|error| error.into_inner()),
            );
        } else {
            drop(
                self.state
                    .changed
                    .wait_timeout(state, Duration::from_millis(100))
                    .unwrap_or_else(|error| error.into_inner()),
            );
        }
    }
}

/// Inbound immutable mutation category.
#[derive(Clone, Copy)]
enum Mutation {
    Edit,
    Delete,
    Reaction,
}

fn worker_loop(ext: Arc<Extension>) {
    #[cfg(test)]
    struct Exit<'a>(&'a dyn ZulipClient);
    #[cfg(test)]
    impl Drop for Exit<'_> {
        fn drop(&mut self) {
            self.0.worker_exited();
        }
    }
    #[cfg(test)]
    let _exit = Exit(ext.client.as_ref());
    let mut backoff = INITIAL_RECONNECT_BACKOFF;
    loop {
        let (cfg, queue, generation, registration_generation) = {
            let mut state = ext.state.lock();
            while !state.shutdown_requested
                && (state.config.is_none()
                    || state.queue.is_none()
                    || state.registered_agents.is_empty())
            {
                state = ext
                    .state
                    .changed
                    .wait(state)
                    .unwrap_or_else(|error| error.into_inner());
            }
            if state.shutdown_requested || ext.shutdown.load(Ordering::Relaxed) {
                return;
            }
            (
                state.config.clone().expect("worker config"),
                state.queue.clone().expect("worker queue"),
                state.config_generation,
                state.registration_generation,
            )
        };
        {
            let mut state = ext.state.lock();
            if let Some(checkpoint) = state.checkpoint.as_mut()
                && let Err(error) = checkpoint.advance()
            {
                tracing::warn!(target: LOG_TARGET, category = %error, "Zulip checkpoint write failed");
                drop(state);
                wait_for_lifecycle_change(
                    &ext,
                    generation,
                    registration_generation,
                    INITIAL_RECONNECT_BACKOFF,
                );
                continue;
            }
        }
        if ext
            .state
            .lock()
            .checkpoint
            .as_ref()
            .is_some_and(CheckpointRuntime::catch_up_needed)
            && let Err(error) =
                ext.catch_up_messages(&cfg, &queue, generation, registration_generation)
        {
            tracing::warn!(target: LOG_TARGET, category = error.diagnostic(), "Zulip message catch-up failed");
            wait_for_lifecycle_change(
                &ext,
                generation,
                registration_generation,
                INITIAL_RECONNECT_BACKOFF,
            );
            continue;
        }
        {
            let state = ext.state.lock();
            if state
                .checkpoint
                .as_ref()
                .is_some_and(CheckpointRuntime::catch_up_needed)
            {
                drop(state);
                ext.wait_for_checkpoint_progress(generation, registration_generation);
                continue;
            }
        }
        match ext.client.get_events(
            &cfg,
            &queue.queue_id,
            queue.last_event_id,
            queue.poll_request_timeout,
        ) {
            Ok(events) => {
                backoff = INITIAL_RECONNECT_BACKOFF;
                let last_id = process_event_batch(
                    &ext,
                    events,
                    queue.last_event_id,
                    generation,
                    registration_generation,
                    queue.bot_user_id,
                );
                if ext.output.mandatory_output_failed() {
                    return;
                }
                let mut state = ext.state.lock();
                if state.config_generation == generation
                    && state.registration_generation == registration_generation
                    && let Some(current) = state
                        .queue
                        .as_mut()
                        .filter(|current| current.queue_id == queue.queue_id)
                {
                    current.last_event_id = last_id;
                }
            }
            Err(ApiError::QueueExpired) => {
                handle_queue_expiry(&ext, &cfg, generation, registration_generation);
            }
            Err(error) => {
                tracing::warn!(target: LOG_TARGET, category = error.diagnostic(), "Zulip long poll failed");
                let delay = if let ApiError::RateLimited { retry, .. } = error {
                    retry
                } else {
                    let delay = backoff;
                    backoff = (backoff * 2).min(MAX_RECONNECT_BACKOFF);
                    delay
                };
                wait_for_lifecycle_change(&ext, generation, registration_generation, delay);
            }
        }
    }
}

/// Process one ordered live-event batch and return only its safely published
/// cursor prefix.
fn process_event_batch(
    ext: &Extension,
    events: Vec<serde_json::Value>,
    mut last_id: i64,
    generation: u64,
    registration_generation: u64,
    bot_user_id: u64,
) -> i64 {
    for event in events {
        let Some(id) = event.get("id").and_then(serde_json::Value::as_i64) else {
            continue;
        };
        if id <= last_id {
            continue;
        }
        if event.get("type").and_then(serde_json::Value::as_str) == Some("message") {
            ext.observe_created_message(event, generation, registration_generation, bot_user_id);
        } else {
            ext.process_event(event, generation, registration_generation, bot_user_id);
        }
        if ext.output.mandatory_output_failed() {
            break;
        }
        last_id = id;
    }
    last_id
}

fn wait_for_lifecycle_change(
    ext: &Extension,
    generation: u64,
    registration_generation: u64,
    delay: Duration,
) {
    let state = ext.state.lock();
    let _ = ext
        .state
        .changed
        .wait_timeout_while(state, delay, |state| {
            !state.shutdown_requested
                && state.config_generation == generation
                && state.registration_generation == registration_generation
        })
        .unwrap_or_else(|error| error.into_inner());
}

fn handle_queue_expiry(
    ext: &Extension,
    cfg: &RuntimeConfig,
    generation: u64,
    registration_generation: u64,
) {
    {
        let mut state = ext.state.lock();
        if state.config_generation != generation
            || state.registration_generation != registration_generation
        {
            return;
        }
        state.queue = None;
    }
    if cfg.offline_message_catch_up {
        ext.output
            .notice("Zulip event queue expired; reconnecting before created-message catch-up.");
    } else {
        ext.output
            .notice("Zulip event queue expired; reconnecting live. Messages may have been missed.");
    }
    let mut backoff = INITIAL_RECONNECT_BACKOFF;
    loop {
        let prepared = ext.acquire_queue(cfg);
        let mut state = ext.state.lock();
        if state.shutdown_requested
            || state.config_generation != generation
            || state.registration_generation != registration_generation
        {
            return;
        }
        match prepared {
            Ok((cfg, queue)) => {
                state.config = Some(cfg);
                state.queue = Some(queue);
                if let Some(checkpoint) = state.checkpoint.as_mut() {
                    checkpoint.set_more_history(true);
                }
                ext.state.changed.notify_all();
                return;
            }
            Err(error) => {
                log_queue_registration_failure(&error);
            }
        }
        let (state_after_wait, _) = ext
            .state
            .changed
            .wait_timeout(state, backoff)
            .unwrap_or_else(|error| error.into_inner());
        state = state_after_wait;
        if state.shutdown_requested
            || state.config_generation != generation
            || state.registration_generation != registration_generation
        {
            return;
        }
        drop(state);
        backoff = (backoff * 2).min(MAX_RECONNECT_BACKOFF);
    }
}

/// Log one content-free live queue re-registration failure.
fn log_queue_registration_failure(error: &ApiError) {
    tracing::warn!(target: LOG_TARGET, category = error.diagnostic(), "Zulip queue registration failed");
}

fn admitted_conversation(
    cfg: &RuntimeConfig,
    message: &serde_json::Value,
    sender_id: u64,
    bot_user_id: u64,
    mentioned: bool,
) -> Option<Conversation> {
    let kind = message.get("type").and_then(serde_json::Value::as_str)?;
    if kind == "private" || kind == "direct" {
        if !cfg.receive_direct_messages {
            return None;
        }
        let mut users = direct_participants(cfg, message, sender_id, bot_user_id)?;
        users.sort_unstable();
        let mut hasher = blake3::Hasher::new_keyed(&cfg.id_key);
        hasher.update(b"tau-ext-zulip/direct-conversation/v1\0");
        for user in &users {
            hasher.update(&user.to_le_bytes());
        }
        let stable_id = format!("zulip-direct:{}", hasher.finalize().to_hex());
        return Some(Conversation {
            route: NativeRoute::Direct(users),
            stable_id,
            alias: None,
        });
    }
    if kind != "stream" {
        return None;
    }
    let stream_id = message
        .get("stream_id")
        .and_then(serde_json::Value::as_u64)?;
    let topic = message
        .get("subject")
        .or_else(|| message.get("topic"))
        .and_then(serde_json::Value::as_str)?;
    let route = cfg.routes.iter().find(|route| {
        route.stream_id == stream_id
            && route
                .topic
                .as_deref()
                .is_none_or(|expected| expected == topic)
            && route.receive.is_some()
    })?;
    if route.receive == Some(ReceiveMode::MentionsOnly) && !mentioned {
        return None;
    }
    Some(stream_conversation(cfg, route, topic))
}

/// Parse and validate the complete participant evidence for one direct message.
fn direct_participants(
    cfg: &RuntimeConfig,
    message: &serde_json::Value,
    sender_id: u64,
    bot_user_id: u64,
) -> Option<Vec<u64>> {
    let recipients = message
        .get("display_recipient")
        .and_then(serde_json::Value::as_array)?;
    if MAX_DIRECT_NON_BOT_PARTICIPANTS + 1 < recipients.len() {
        return None;
    }

    let mut participant_ids = HashSet::with_capacity(recipients.len());
    for recipient in recipients {
        let id = recipient
            .as_object()?
            .get("id")
            .and_then(serde_json::Value::as_u64)
            .filter(|id| *id != 0)?;
        if !participant_ids.insert(id) {
            return None;
        }
    }
    if !participant_ids.contains(&bot_user_id) || !participant_ids.contains(&sender_id) {
        return None;
    }

    let users = participant_ids
        .into_iter()
        .filter(|id| *id != bot_user_id)
        .collect::<Vec<_>>();
    if users.is_empty() || MAX_DIRECT_NON_BOT_PARTICIPANTS < users.len() {
        return None;
    }
    if users.iter().any(|id| !cfg.allowed_user_ids.contains(id)) {
        return None;
    }
    Some(users)
}

fn stream_conversation(cfg: &RuntimeConfig, route: &StreamRoute, topic: &str) -> Conversation {
    let mut hasher = blake3::Hasher::new_keyed(&cfg.id_key);
    hasher.update(b"tau-ext-zulip/stream-conversation/v1\0");
    let stream_id = route.stream_id;
    hasher.update(&stream_id.to_le_bytes());
    hasher.update(topic.as_bytes());
    Conversation {
        route: NativeRoute::Stream {
            stream_id,
            topic: topic.to_owned(),
        },
        stable_id: format!("zulip-stream:{}", hasher.finalize().to_hex()),
        alias: Some(route.name.clone()),
    }
}

/// Build the opaque conversation record for one configured direct destination.
fn direct_conversation(cfg: &RuntimeConfig, route: &DirectRoute) -> Conversation {
    let mut hasher = blake3::Hasher::new_keyed(&cfg.id_key);
    hasher.update(b"tau-ext-zulip/direct-conversation/v1\0");
    hasher.update(&route.recipient().to_le_bytes());
    Conversation {
        route: NativeRoute::Direct(vec![route.recipient()]),
        stable_id: format!("zulip-direct:{}", hasher.finalize().to_hex()),
        alias: Some(route.alias().to_owned()),
    }
}

/// Validate an agent-selected topic, including Zulip's canonical empty topic.
fn validate_agent_topic(value: &str) -> Result<(), String> {
    let valid = value.is_empty()
        || (!value.trim().is_empty()
            && value.len() < 257
            && !value.chars().any(tau_proto::requires_visible_escape));
    valid.then_some(()).ok_or_else(|| {
        "zulip agent-chosen topics must be empty or visible and at most 256 bytes".to_owned()
    })
}

fn message_fact_id(cfg: &RuntimeConfig, native_id: u64) -> MessageFactId {
    let mut hasher = blake3::Hasher::new_keyed(&cfg.id_key);
    hasher.update(b"tau-ext-zulip/message/v1\0");
    hasher.update(native_id.to_string().as_bytes());
    MessageFactId::new(format!("zulip-message:{}", hasher.finalize().to_hex()))
}

fn sender_party(cfg: &RuntimeConfig, sender_id: u64, alias: Option<String>) -> MessageParty {
    let mut hasher = blake3::Hasher::new_keyed(&cfg.id_key);
    hasher.update(b"tau-ext-zulip/sender/v1\0");
    hasher.update(sender_id.to_string().as_bytes());
    MessageParty {
        stable_id: format!("zulip-sender:{}", hasher.finalize().to_hex()),
        display_name: alias,
        sender_auth: Some(MessageSenderAuth::VerifiedAllowlisted),
    }
}

fn run_with_client<R, W>(
    reader: R,
    writer: W,
    client: Arc<dyn ZulipClient>,
) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    let mut runtime = tau_client::TauExtensionRunner::new(ZulipExtension)
        .start_manual_loop_deferred_startup_with_state(reader, writer, move |handle| {
            ZulipRuntime {
                ext: Extension::new(client, handle, ToolNames::logical()),
            }
        })?;
    runtime.state().ext.output.install_waker(runtime.waker());
    let configure = match read_initial_config(&mut runtime) {
        Ok(Some(configure)) => configure,
        Ok(None) => {
            runtime.finish_detached().ext.request_shutdown_detached();
            return Ok(());
        }
        Err(error) => {
            runtime.state().ext.request_shutdown();
            let _ = runtime.finish();
            return Err(error);
        }
    };
    match configure_initial(&configure, &mut runtime) {
        Ok(names) => {
            if let Err(error) = send_startup(&mut runtime, &names) {
                runtime.state().ext.request_shutdown();
                let _ = runtime.finish();
                return Err(Box::new(error));
            }
        }
        Err(error) => {
            runtime.state().ext.clear_config();
            if let Err(client_error) = runtime.handle().config_error(error.to_string()) {
                runtime.state().ext.request_shutdown();
                let _ = runtime.finish();
                return Err(Box::new(client_error));
            }
        }
    }
    let loop_result: ClientResult<bool> = 'drive: loop {
        if let Err(error) = runtime.state().ext.output.check_mandatory_output() {
            break Err(error);
        }
        let poll = match runtime.try_recv() {
            Ok(poll) => poll,
            Err(error) => break Err(error),
        };
        match poll {
            ManualRuntimePoll::Message(tau_proto::HarnessOutputMessage::Configure(configure)) => {
                if let Err(error) = handle_configure(runtime.state(), configure) {
                    break Err(error);
                }
            }
            ManualRuntimePoll::Message(tau_proto::HarnessOutputMessage::Deliver(delivery))
                if !delivery.replay =>
            {
                match *delivery.event {
                    Event::ToolStarted(invoke)
                        if runtime.state().ext.handles_tool(invoke.tool_name.as_str()) =>
                    {
                        if let Err(error) = runtime.state().ext.dispatch_tool_checked(invoke) {
                            break 'drive Err(error);
                        }
                    }
                    event => handle_live_event(runtime.state(), event),
                }
            }
            ManualRuntimePoll::Message(tau_proto::HarnessOutputMessage::Disconnect(_)) => {
                break Ok(true);
            }
            ManualRuntimePoll::InputClosed => break Ok(false),
            ManualRuntimePoll::Empty => runtime.wait_for_wake(),
            _ => {}
        }
    };
    let disconnected = match loop_result {
        Ok(disconnected) => disconnected,
        Err(error) => {
            let mandatory_failed = runtime.state().ext.output.check_mandatory_output().is_err();
            if mandatory_failed {
                runtime.state().ext.request_shutdown();
            } else {
                runtime.state().ext.request_shutdown_detached();
            }
            let _ = runtime.finish();
            return Err(Box::new(error));
        }
    };
    if disconnected {
        runtime.finish_detached().ext.request_shutdown_detached();
        Ok(())
    } else {
        runtime.state().ext.request_shutdown_detached();
        let finish = runtime.finish();
        finish
            .map(|_| ())
            .map_err(|error| Box::new(error) as Box<dyn Error>)
    }
}

/// Marker registering the message-bridge capability.
struct ZulipExtension;
impl TauExtension for ZulipExtension {
    type State = ZulipRuntime;
    fn name(&self) -> &'static str {
        "tau-ext-zulip"
    }
    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder.message_bridge();
    }
}

/// State owned by the manual tau-client runtime.
struct ZulipRuntime {
    /// Shared bridge implementation and worker coordination.
    ext: Extension,
}

fn read_initial_config(
    runtime: &mut tau_client::ManualExtensionRuntime<ZulipRuntime>,
) -> Result<Option<tau_proto::Configure>, Box<dyn Error>> {
    loop {
        match runtime.recv()? {
            ManualRuntimeInput::Message(tau_proto::HarnessOutputMessage::Configure(configure)) => {
                runtime.dispatch_one(tau_proto::HarnessOutputMessage::Configure(
                    configure.clone(),
                ))?;
                return Ok(Some(configure));
            }
            ManualRuntimeInput::Message(tau_proto::HarnessOutputMessage::Disconnect(_))
            | ManualRuntimeInput::InputClosed => return Ok(None),
            _ => {}
        }
    }
}

fn configure_initial(
    configure: &tau_proto::Configure,
    runtime: &mut tau_client::ManualExtensionRuntime<ZulipRuntime>,
) -> Result<ToolNames, Box<dyn Error>> {
    let cfg: ExtConfig = configure.config.deserialized()?;
    let mut cfg = cfg.validate(&configure.secrets)?;
    cfg.state_dir = configure.state_dir.clone();
    let names = ToolNames::from_scope(runtime.handle().tool_name_scope()?)?;
    runtime.state_mut().ext.tool_names = names.clone();
    runtime
        .state()
        .ext
        .apply_config(cfg, configure.instance_name.clone());
    Ok(names)
}

fn handle_configure(runtime: &ZulipRuntime, configure: tau_proto::Configure) -> ClientResult<()> {
    let result = configure
        .config
        .deserialized::<ExtConfig>()
        .map_err(|error| error.to_string())
        .and_then(|cfg| cfg.validate(&configure.secrets))
        .map(|mut cfg| {
            cfg.state_dir = configure.state_dir.clone();
            cfg
        });
    match result {
        Ok(cfg) => runtime.ext.apply_config(cfg, configure.instance_name),
        Err(error) => {
            runtime.ext.clear_config();
            if let Some(handle) = runtime.ext.output.client_handle() {
                handle.config_error(error)?;
            }
        }
    }
    Ok(())
}

fn handle_live_event(runtime: &ZulipRuntime, event: Event) {
    match event {
        Event::MessageDelivered(fact) => {
            let mut state = runtime.ext.state.lock();
            if state
                .publisher_name
                .as_ref()
                .is_none_or(|publisher| publisher.as_str() != fact.publisher_extension_id.as_str())
            {
                return;
            }
            if let Some(checkpoint) = state.checkpoint.as_mut()
                && checkpoint.acknowledge(&fact.message_id)
            {
                if let Err(error) = checkpoint.advance() {
                    tracing::warn!(target: LOG_TARGET, category = %error, "Zulip checkpoint write failed");
                }
                runtime.ext.state.changed.notify_all();
            }
        }
        Event::AgentDisplayNameSet(value) => {
            runtime
                .ext
                .state
                .lock()
                .agent_labels
                .insert(value.agent_id, value.display_name);
        }
        Event::AgentStarted(value) => {
            if let Some(name) = value.display_name {
                runtime
                    .ext
                    .state
                    .lock()
                    .agent_labels
                    .insert(value.agent_id, name);
            }
        }
        Event::SessionAgentUnloaded(value) => {
            let _authority = runtime.ext.publication_authority.retire();
            let mut state = runtime.ext.state.lock();
            state.unregister_agent(&value.agent_id);
            state.registration_generation = state.registration_generation.wrapping_add(1);
            runtime.ext.state.changed.notify_all();
        }
        Event::SessionShutdown(_) => {
            let _authority = runtime.ext.publication_authority.retire();
            let mut state = runtime.ext.state.lock();
            state.clear_authority();
            runtime.ext.state.changed.notify_all();
        }
        _ => {}
    }
}

fn send_startup(
    runtime: &mut tau_client::ManualExtensionRuntime<ZulipRuntime>,
    names: &ToolNames,
) -> ClientResult<()> {
    runtime.startup_subscribe([
        tau_proto::EventSelector::Exact(tau_proto::EventName::TOOL_STARTED),
        tau_proto::EventSelector::Exact(tau_proto::EventName::AGENT_DISPLAY_NAME_SET),
        tau_proto::EventSelector::Exact(tau_proto::EventName::AGENT_STARTED),
        tau_proto::EventSelector::Exact(tau_proto::EventName::SESSION_AGENT_UNLOADED),
        tau_proto::EventSelector::Exact(tau_proto::EventName::SESSION_SHUTDOWN),
        tau_proto::EventSelector::Exact(tau_proto::EventName::MESSAGE_DELIVERED),
    ])?;
    for tool in [
        register_spec(names),
        conversations_spec(names),
        send_spec(names),
        react_spec(names),
    ] {
        runtime.startup_local_tool(tau_proto::ToolRegistrationDeclared {
            tool,
            tool_group: Some(tool_group(names)),
            prompt_fragment: None,
        })?;
    }
    runtime.startup_ready(Some("zulip ready".to_owned()))
}

fn base_spec(
    name: &str,
    model_name: tau_proto::ToolName,
    description: String,
    parameters: serde_json::Value,
    tag: &str,
    examples: Vec<ToolExample>,
) -> ToolSpec {
    ToolSpec {
        name: tau_proto::ToolName::new(name),
        model_visible_name: Some(model_name),
        description: Some(description),
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(parameters),
        format: None,
        tags: vec![tau_proto::ToolTag::new(tag)],
        enabled_by_default: false,
        background_support: None,
        examples,
    }
}

fn register_spec(names: &ToolNames) -> ToolSpec {
    base_spec(
        REGISTER_TOOL_NAME,
        names.register.clone(),
        format!(
            "Register or unregister this agent for messages through the `{}` Zulip bridge.",
            names.namespace
        ),
        serde_json::json!({"type":"object","properties":{"enabled":{"type":"boolean"}},"required":["enabled"],"additionalProperties":false}),
        REGISTER_TOOL_TAG,
        vec![example(
            "register",
            CborValue::Map(vec![(
                CborValue::Text("enabled".to_owned()),
                CborValue::Bool(true),
            )]),
        )],
    )
}
fn conversations_spec(names: &ToolNames) -> ToolSpec {
    base_spec(
        CONVERSATIONS_TOOL_NAME,
        names.conversations.clone(),
        "List proactive Zulip aliases, including any configured stream destination \
         that explicitly lets an agent choose its topic."
            .to_owned(),
        serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
        CONVERSATIONS_TOOL_TAG,
        vec![example("discover", CborValue::Map(vec![]))],
    )
}
fn send_spec(names: &ToolNames) -> ToolSpec {
    base_spec(
        SEND_TOOL_NAME,
        names.send.clone(),
        "Send Zulip Markdown using a Tau-issued source reply reference or configured \
         destination alias. Supply `topic` only with a discovered destination marked \
         `agent_chosen_topic`; `topic: \"\"` sends to Zulip general chat."
            .to_owned(),
        serde_json::json!({
            "type":"object",
            "properties":{
                "message":{"type":"string"},
                "reply_to":{"type":"string"},
                "destination":{"type":"string"},
                "topic":{"type":"string"}
            },
            "required":["message"],
            "additionalProperties":false
        }),
        SEND_TOOL_TAG,
        vec![example(
            "reply",
            CborValue::Map(vec![
                (
                    CborValue::Text("message".to_owned()),
                    CborValue::Text("Thanks".to_owned()),
                ),
                (
                    CborValue::Text("reply_to".to_owned()),
                    CborValue::Text("zulip-message:…".to_owned()),
                ),
            ]),
        )],
    )
}
fn react_spec(names: &ToolNames) -> ToolSpec {
    base_spec(
        REACT_TOOL_NAME,
        names.react.clone(),
        "Add or remove one Zulip emoji reaction using a same-agent Tau-issued message reference."
            .to_owned(),
        serde_json::json!({"type":"object","properties":{"message_ref":{"type":"string"},"emoji":{"type":"string"},"action":{"enum":["add","remove"]}},"required":["message_ref","emoji","action"],"additionalProperties":false}),
        REACT_TOOL_TAG,
        vec![example(
            "react",
            CborValue::Map(vec![
                (
                    CborValue::Text("message_ref".to_owned()),
                    CborValue::Text("zulip-message:…".to_owned()),
                ),
                (
                    CborValue::Text("emoji".to_owned()),
                    CborValue::Text("thumbs_up".to_owned()),
                ),
                (
                    CborValue::Text("action".to_owned()),
                    CborValue::Text("add".to_owned()),
                ),
            ]),
        )],
    )
}

fn tool_group(names: &ToolNames) -> tau_proto::ToolGroup {
    tau_proto::ToolGroup {
        name: names.group.clone(),
        prompt_fragment: None,
    }
}
fn example(id: &str, arguments: CborValue) -> ToolExample {
    ToolExample {
        id: id.to_owned(),
        title: None,
        arguments,
        note: None,
        subcommand: None,
    }
}

fn validate_fields(value: &CborValue, allowed: &[&str]) -> Result<(), String> {
    let CborValue::Map(entries) = value else {
        return Err("arguments must be an object".to_owned());
    };
    for (key, _) in entries {
        let CborValue::Text(key) = key else {
            return Err("argument field names must be strings".to_owned());
        };
        if !allowed.contains(&key.as_str()) {
            return Err(format!("unknown argument `{key}`"));
        }
    }
    Ok(())
}
fn bool_field(value: &CborValue, field: &str) -> Result<bool, String> {
    field_value(value, field).and_then(|value| match value {
        CborValue::Bool(value) => Ok(*value),
        _ => Err(format!("`{field}` must be a boolean")),
    })
}
fn string_field(value: &CborValue, field: &str) -> Result<String, String> {
    field_value(value, field).and_then(|value| match value {
        CborValue::Text(value) => Ok(value.clone()),
        _ => Err(format!("`{field}` must be a string")),
    })
}
fn optional_string_field(value: &CborValue, field: &str) -> Result<Option<String>, String> {
    match field_value(value, field) {
        Ok(CborValue::Text(value)) => Ok(Some(value.clone())),
        Ok(_) => Err(format!("`{field}` must be a string")),
        Err(_) => Ok(None),
    }
}
fn field_value<'a>(value: &'a CborValue, field: &str) -> Result<&'a CborValue, String> {
    let CborValue::Map(entries) = value else {
        return Err("arguments must be an object".to_owned());
    };
    entries
        .iter()
        .find_map(|(key, value)| {
            matches!(key, CborValue::Text(name) if name == field).then_some(value)
        })
        .ok_or_else(|| format!("missing required argument `{field}`"))
}

fn tool_result(invoke: ToolStarted, text: String) -> Event {
    Event::ToolResult(ToolResult {
        presentation: Default::default(),
        call_id: invoke.call_id,
        tool_name: invoke.tool_name,
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text(text),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: Some(ToolUseState {
            status: ToolUseStatus::Success,
            status_text: "ok".to_owned(),
            ..Default::default()
        }),
        originator: invoke.originator,
    })
}
fn tool_error(invoke: ToolStarted, message: String) -> Event {
    Event::ToolError(ToolError {
        presentation: Default::default(),
        call_id: invoke.call_id,
        tool_name: invoke.tool_name,
        tool_type: tau_proto::ToolType::Function,
        display: Some(ToolUseState {
            status: ToolUseStatus::Error,
            status_text: message.clone(),
            ..Default::default()
        }),
        message,
        details: Some(invoke.arguments),
        originator: invoke.originator,
    })
}

#[cfg(test)]
mod tests;
