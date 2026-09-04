//! Personal Telegram bridge extension for Tau agents.
//!
//! By default the extension exposes `telegram_register` and `telegram_send`
//! tools. It keeps desired listener registrations in Session-scope extension
//! data, retains active routes in memory, and uses the Telegram Bot API only
//! after an agent registers or another Telegram action needs the client.
//! Update-stream ownership follows
//! `SPEC-tau-ext-telegram-stream-owner`, while instance-specific tool
//! names follow the workspace-wide `SPEC-extension-tool-prefixes`.
//! Shared-token multi-session ownership follows
//! `SPEC-tau-telegram-gateway`.
//! The standalone daemon and sidecar split is described by
//! `ARCH-tau-telegram-gateway`.

use ureq::tls as path_ureq_tls;

mod desired_registrations;
mod gateway;
mod gateway_auth;
mod gateway_client;
mod gateway_exit;
mod gateway_supervisor;
mod live_checkpoint;
mod output;
mod pending_retry_backoff;
mod stream_owner;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::io::{Error as IoError, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(test)]
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use desired_registrations::{DesiredRegistrationStorage, DesiredRegistrationStoreError};
use gateway_client::{
    GatewayClient, GatewayClientConfig, GatewayMessageDelivery, GatewaySocketResponse,
};
use gateway_supervisor::{GatewaySupervisor, gateway_response_requires_reconnect};
use live_checkpoint::{
    ExistingUpdate, LiveCheckpoints, RoutedUpdate, TelegramReportId, TelegramUpdateId,
    TelegramUpdateOffset,
};
pub(crate) use output::Output;
#[cfg(test)]
pub(crate) use output::SATURATION_HOOK;
use pending_retry_backoff::PendingRetryBackoff;
use stream_owner::{
    StreamIdentity, TelegramWebhookInfo, UpdateStreamLock, telegram_contention_diagnostic,
    webhook_active_message,
};
use tau_client::{
    ClientResult, ExtensionBuilder, ManualRuntimeInput, ManualRuntimePoll, TauExtension,
};
use tau_proto::{
    AgentId, CborValue, Event, MessageAgentTarget, MessageConversation, MessageDelivered,
    MessageFactId, MessageParty, MessageSenderAuth, MessageSent, NoticeLevel,
    RawMessagePublisherId, ToolError, ToolExample, ToolProgress, ToolResult, ToolSpec, ToolStarted,
    ToolUseState, ToolUseStatus,
};

/// Tracing target used by this extension.
pub const LOG_TARGET: &str = "telegram";

/// Maximum bytes in one gateway protocol response, including its newline.
const MAX_GATEWAY_RESPONSE_BYTES: usize = 64 * 1024;

/// Logical tool name for registering the current agent as a Telegram listener.
pub const REGISTER_TOOL_NAME: &str = "telegram_register";

/// Logical tool name for sending a Telegram message from a registered agent.
pub const SEND_TOOL_NAME: &str = "telegram_send";

/// Logical tool group name shared by Telegram bridge tools.
pub const TOOL_GROUP_NAME: &str = "telegram";

/// Tag marking tools that register an agent with the Telegram bridge.
pub const REGISTER_TOOL_TAG: &str = "telegram:register";

/// Tag marking tools that send messages through the Telegram bridge.
pub const SEND_TOOL_TAG: &str = "telegram:send";

const DEFAULT_API_BASE: &str = "https://api.telegram.org";
const DEFAULT_POLL_TIMEOUT_SECONDS: u64 = 25;
const HTTP_TIMEOUT: Duration = Duration::from_secs(35);
const MAX_GATEWAY_OUTBOUND_MESSAGE_BYTES: usize = 3500;
/// Maximum accepted size of one successful Telegram Bot API response body.
///
/// The reader requests one additional byte so it accepts this exact size and
/// rejects a larger body before JSON decoding.
const MAX_SUCCESSFUL_RESPONSE_BODY_BYTES: u64 = 10 * 1024 * 1024;

/// Run the Telegram extension over stdio.
pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    tau_client::init_logging_for(LOG_TARGET);
    run(std::io::stdin(), std::io::stdout())
}

/// Run the standalone Telegram gateway daemon from process command-line
/// arguments and environment variables.
pub fn run_gateway_from_env() -> std::process::ExitCode {
    tau_client::init_logging_for(LOG_TARGET);
    gateway::run_from_env()
}

/// Run the Telegram extension over an arbitrary transport.
pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    run_with_client(reader, writer, Arc::new(HttpTelegramClient::default()))
}

/// Small Bot API surface used by the extension and faked by unit tests.
trait TelegramClient: Send + Sync + 'static {
    /// Test-observable poller retirement hook.
    #[cfg(test)]
    fn poller_exited(&self) {}
    /// Fetch webhook status without consuming the update stream.
    fn get_webhook_info(&self, cfg: &RuntimeConfig) -> Result<TgWebhookInfo, TelegramApiFailure>;

    /// Fetch message updates from Telegram using the configured poll timeout.
    fn get_updates(
        &self,
        cfg: &RuntimeConfig,
        offset: Option<i64>,
    ) -> Result<Vec<TgUpdate>, TelegramApiFailure>;

    /// Send a plain text message to one configured or linked chat.
    fn send_message(
        &self,
        cfg: &RuntimeConfig,
        chat_id: i64,
        text: &str,
    ) -> Result<(), TelegramApiFailure>;
}

/// Bot API failure retaining the stable facts needed by gateway exit policy.
#[derive(Clone, Debug, Eq, PartialEq)]
enum TelegramApiFailure {
    /// DNS, connection, TLS, or timeout failure before an HTTP response.
    Transport,
    /// Non-success HTTP response with redacted, bounded response text.
    Http {
        /// HTTP status code returned by the Bot API endpoint.
        status: u16,
        /// Redacted response excerpt for operator diagnosis.
        message: String,
    },
    /// A successful response that violated the expected Bot API shape.
    Protocol(String),
}

impl fmt::Display for TelegramApiFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport => formatter.write_str("Telegram transport error"),
            Self::Http { status, message } => {
                write!(formatter, "Telegram returned HTTP {status}: {message}")
            }
            Self::Protocol(message) => message.fmt(formatter),
        }
    }
}

impl std::error::Error for TelegramApiFailure {}

/// Validated runtime configuration, including resolved secret values.
#[derive(Clone)]
struct RuntimeConfig {
    /// Resolved bot token. Never log this value.
    bot_token: String,
    /// Telegram user ids allowed to interact with this bridge.
    allowed_user_ids: HashSet<i64>,
    /// Optional fixed chat id for outgoing messages.
    configured_chat_id: Option<i64>,
    /// Bot API base URL.
    api_base: String,
    /// Long-poll timeout passed to Telegram.
    poll_timeout_seconds: u64,
}

/// Runtime mode selected for one extension instance.
#[derive(Clone)]
enum BridgeMode {
    /// Legacy single-harness mode where the sidecar owns `getUpdates`.
    LocalPoll(RuntimeConfig),
    /// Gateway-client mode where a standalone daemon owns Telegram polling.
    GatewayClient(GatewayClientConfig),
}

impl std::fmt::Debug for BridgeMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LocalPoll(_) => f.write_str("LocalPoll(<redacted>)"),
            Self::GatewayClient(config) => f.debug_tuple("GatewayClient").field(config).finish(),
        }
    }
}

impl From<RuntimeConfig> for BridgeMode {
    fn from(cfg: RuntimeConfig) -> Self {
        Self::LocalPoll(cfg)
    }
}

/// Configured sidecar mode in `harness.yaml`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum ExtMode {
    /// Legacy local polling mode.
    #[default]
    LocalPoll,
    /// No-poll client mode backed by the standalone gateway daemon.
    GatewayClient,
}

impl RuntimeConfig {
    /// Telegram update offsets are scoped to the Bot API endpoint plus bot
    /// token; switching either value starts reading a different update stream.
    fn uses_same_update_stream_as(&self, other: &Self) -> bool {
        self.api_base == other.api_base && self.bot_token == other.bot_token
    }

    /// Return the shared stream-owner identity for this Bot API stream.
    fn stream_identity(&self) -> StreamIdentity<'_> {
        StreamIdentity::new(&self.api_base, &self.bot_token)
    }
}

/// Tool and group names published by one Telegram extension instance.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ToolNames {
    /// Namespace prefix used before `_register` and `_send`.
    namespace: String,
    /// Tool name for registering the current agent as a Telegram listener.
    register: tau_proto::ToolName,
    /// Tool name for sending a Telegram message from a registered agent.
    send: tau_proto::ToolName,
    /// Tool group name for policy that enables this instance's Telegram tools.
    group: tau_proto::ToolGroupName,
}

impl ToolNames {
    /// Build final names exclusively from the generic SDK scope.
    fn from_scope(scope: &tau_client::ToolNameScope) -> ClientResult<Self> {
        Ok(Self {
            namespace: scope
                .wire_group_name(&tau_proto::ToolGroupName::new(TOOL_GROUP_NAME))?
                .to_string(),
            register: scope.wire_tool(REGISTER_TOOL_NAME)?,
            send: scope.wire_tool(SEND_TOOL_NAME)?,
            group: scope.wire_group_name(&tau_proto::ToolGroupName::new(TOOL_GROUP_NAME))?,
        })
    }

    /// Build unprefixed logical names for direct unit-test instances.
    fn logical() -> Self {
        Self {
            namespace: TOOL_GROUP_NAME.to_owned(),
            register: tau_proto::ToolName::new(REGISTER_TOOL_NAME),
            send: tau_proto::ToolName::new(SEND_TOOL_NAME),
            group: tau_proto::ToolGroupName::new(TOOL_GROUP_NAME),
        }
    }
}

/// Raw deserialized extension config from `harness.yaml`.
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ExtConfig {
    /// Runtime mode for this Telegram sidecar.
    mode: ExtMode,
    /// Secret name carrying the Telegram bot token.
    bot_token_secret: Option<String>,
    /// Telegram user ids allowed to drive Tau agents.
    allowed_user_ids: Vec<i64>,
    /// Optional fixed chat for outgoing messages.
    chat_id: Option<i64>,
    /// Optional Telegram API base URL, mostly for tests.
    api_base: Option<String>,
    /// Optional long-poll timeout in seconds.
    poll_timeout_seconds: Option<u64>,
    /// Unix socket used in `gateway_client` mode.
    gateway_socket_path: Option<PathBuf>,
    /// Declared secret name carrying the gateway authentication key.
    gateway_client_secret: Option<String>,
}

impl ExtConfig {
    fn validate(
        self,
        secrets: &BTreeMap<String, tau_proto::SecretValue>,
    ) -> Result<BridgeMode, String> {
        if self.mode == ExtMode::GatewayClient {
            let socket_path = self
                .gateway_socket_path
                .filter(|path| !path.as_os_str().is_empty())
                .ok_or_else(|| {
                    "telegram gateway_client mode requires `gateway_socket_path`".to_owned()
                })?;
            let secret_name = self.gateway_client_secret.ok_or_else(|| {
                "telegram gateway_client mode requires `gateway_client_secret`".to_owned()
            })?;
            let secret = secrets
                .get(&secret_name)
                .map(tau_proto::SecretValue::expose_secret)
                .ok_or_else(|| "telegram gateway client secret is missing".to_owned())?;
            let auth_key = gateway_auth::GatewayAuthKey::parse(secret)?;
            return Ok(BridgeMode::GatewayClient(GatewayClientConfig {
                socket_path,
                auth_key,
                client_generation: gateway_auth::ClientGeneration::random(),
            }));
        }
        let secret_name = self
            .bot_token_secret
            .ok_or_else(|| "telegram config requires `bot_token_secret`".to_owned())?;
        let token = secrets
            .get(&secret_name)
            .map(tau_proto::SecretValue::expose_secret)
            .filter(|token| !token.trim().is_empty())
            .ok_or_else(|| format!("telegram secret `{secret_name}` is missing or empty"))?;
        if self.allowed_user_ids.is_empty() {
            return Err("telegram config requires non-empty `allowed_user_ids`".to_owned());
        }
        let api_base = self
            .api_base
            .unwrap_or_else(|| DEFAULT_API_BASE.to_owned())
            .trim_end_matches('/')
            .to_owned();
        validate_api_base(&api_base)?;
        let poll_timeout_seconds = self
            .poll_timeout_seconds
            .unwrap_or(DEFAULT_POLL_TIMEOUT_SECONDS);
        Ok(BridgeMode::LocalPoll(RuntimeConfig {
            bot_token: token.to_owned(),
            allowed_user_ids: self.allowed_user_ids.into_iter().collect(),
            configured_chat_id: self.chat_id,
            api_base,
            poll_timeout_seconds,
        }))
    }
}

/// A Telegram update containing a message, if present.
#[derive(Clone, Debug, Eq, PartialEq)]
struct TgUpdate {
    /// Telegram update id used for offset advancement.
    update_id: TelegramUpdateId,
    /// Text message payload, or `None` for updates kept only to advance offset.
    message: Option<TgMessage>,
}

/// Result of processing one live local-poll Telegram update.
enum UpdateDisposition {
    /// The update emitted one report that must await its canonical echo.
    Routed(RoutedUpdate),
    /// The update emitted no Tau event and completed at processing return.
    NonRouted,
}

type TgWebhookInfo = TelegramWebhookInfo;

/// Telegram text message details consumed by routing logic.
#[derive(Clone, Debug, Eq, PartialEq)]
struct TgMessage {
    /// Chat id the message arrived in.
    chat_id: i64,
    /// Telegram chat type such as `private`, `group`, or `supergroup`.
    chat_type: Option<String>,
    /// Sending user id.
    user_id: i64,
    /// Human-readable sender label when available.
    from_name: Option<String>,
    /// Optional text. Attachments without captions have no text.
    text: Option<String>,
}

/// Private chat learned through `/start` when no fixed `chat_id` is configured.
#[derive(Clone, Copy)]
struct LinkedChat {
    /// Telegram private chat id used as the reply destination.
    chat_id: i64,
    /// Allowlisted Telegram user id that established this chat link.
    user_id: i64,
}

/// Snapshot of state needed to issue exactly one Telegram poll request.
struct PollRequest {
    /// Runtime configuration captured for this request.
    cfg: RuntimeConfig,
    /// Telegram update offset captured for this request.
    offset: Option<TelegramUpdateOffset>,
    /// Configuration generation captured for stale-response checks.
    config_generation: ConfigGeneration,
    /// Coordination generation observed before this request.
    coordination_generation: TelegramCoordinationGeneration,
    /// Held advisory lock clone that keeps the stream locked until return.
    update_stream_lock: Arc<UpdateStreamLock>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ConfigGeneration(u64);

impl ConfigGeneration {
    fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }
}

/// Process-local generation for poller coordination-relevant state changes.
///
/// This runtime-only authority wakes pending retry waits when configuration,
/// registration, shutdown, or acknowledged-poll progress changes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TelegramCoordinationGeneration(u64);

impl TelegramCoordinationGeneration {
    /// Advances this generation with its existing wrapping overflow behavior.
    #[must_use]
    fn wrapping_next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }
}

#[derive(Default)]
struct State {
    /// Monotonic counter for poller coordination-relevant state changes.
    coordination_generation: TelegramCoordinationGeneration,
    /// Whether the extension is shutting down and local poller waits should
    /// end.
    shutdown_requested: bool,
    config: Option<RuntimeConfig>,
    /// Stable configured publisher identity used in top-level report claims.
    publisher_name: Option<tau_proto::ExtensionName>,
    /// Harness-provided user-scoped state directory for this extension
    /// instance.
    state_dir: Option<std::path::PathBuf>,
    config_generation: ConfigGeneration,
    /// Configuration generation whose gateway supervisor is authoritative.
    gateway_config_generation: Option<ConfigGeneration>,
    registered_agents: HashSet<AgentId>,
    /// Session-scoped desired registrations loaded from durable extension data.
    desired_registrations: BTreeSet<AgentId>,
    /// Loaded membership reconstructed from the current replay window.
    replayed_loaded_agents: HashSet<AgentId>,
    /// Whether the current session's replay boundary authorized route
    /// activation.
    registration_replay_complete: bool,
    /// Whether the immutable session-start boundary has been consumed.
    session_start_observed: bool,
    /// Local registration calls that have reserved update-stream ownership
    /// while checking Telegram webhook status without holding the state mutex.
    pending_local_registrations: usize,
    agent_labels: HashMap<AgentId, String>,
    /// Current local Tau session observed from `session.started`.
    /// Gateway-client mode never announces agent routes until this is
    /// known, and delivery records must match it before local report
    /// submission.
    current_session_id: Option<tau_proto::SessionId>,
    selected_agent_by_chat: HashMap<i64, AgentId>,
    learned_chat: Option<LinkedChat>,
    poller_started: bool,
    /// Held OS advisory lock for the singleton Telegram update stream.
    update_stream_lock: Option<Arc<UpdateStreamLock>>,
    poller_drained_initial_backlog: bool,
    next_update_offset: Option<TelegramUpdateOffset>,
    /// Ordered mixed checkpoints controlling local Telegram cursor advancement.
    live_checkpoints: LiveCheckpoints,
    /// Gateway reports awaiting exact canonical echoes in this live sidecar.
    gateway_pending_deliveries: HashMap<TelegramReportId, GatewayPendingDelivery>,
}

/// Whether a provider batch may process another delivery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessingControl {
    /// Continue with the next provider item.
    Continue,
    /// Stop immediately after mandatory output failure.
    Stop,
}

/// Exact sidecar correlation retained between gateway report and canonical
/// fact.
struct GatewayPendingDelivery {
    /// Session frozen into the durable gateway route.
    session_id: String,
    /// Exact target agent expected on the canonical fact.
    agent_id: AgentId,
    /// Exact message identity expected on the canonical fact.
    message_id: MessageFactId,
    /// Exact configured publisher claim installed on the report.
    publisher_name: tau_proto::ExtensionName,
    /// Whether the exact live canonical echo has already been validated.
    canonical_echo_observed: bool,
}

/// Submit reports for all gateway delivery records targeting the current
/// session.
fn emit_gateway_deliveries(
    state: &SharedState,
    output: &Output,
    gateway_cell: &Mutex<Option<Arc<GatewayClient>>>,
    gateway: Arc<GatewayClient>,
    deliveries: Vec<GatewayMessageDelivery>,
) -> ProcessingControl {
    for delivery in deliveries {
        let Ok(agent_id) = AgentId::parse(&delivery.agent_id) else {
            tracing::warn!(
                target: LOG_TARGET,
                "telegram gateway delivery had invalid agent id"
            );
            continue;
        };
        let mut state_guard = state.lock();
        let gateway_guard = gateway_cell.lock().expect("gateway lock");
        if !gateway_guard
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &gateway))
        {
            return ProcessingControl::Continue;
        }
        let publisher_name = (state_guard
            .current_session_id
            .as_ref()
            .is_some_and(|session_id| delivery.session_id == session_id.as_ref())
            && state_guard.registered_agents.contains(&agent_id))
        .then(|| state_guard.publisher_name.clone())
        .flatten();
        let Some(publisher_name) = publisher_name else {
            tracing::warn!(
                target: LOG_TARGET,
                "telegram gateway delivery targeted a non-live local registration"
            );
            continue;
        };
        let Some(report_id) = TelegramReportId::from_gateway(delivery.request_id.clone()) else {
            tracing::warn!(
                target: LOG_TARGET,
                "telegram gateway delivery had invalid report id"
            );
            continue;
        };
        let message_id = telegram_message_ref(&delivery.conversation_id, &delivery.message_id);
        let mut report = MessageDelivered::new(
            RawMessagePublisherId::new(publisher_name.as_str()),
            MessageAgentTarget::new(agent_id.as_ref()),
            message_id.clone(),
            MessageParty {
                stable_id: telegram_sender_ref(&delivery.sender_id),
                display_name: bounded_display_name(&delivery.source),
                sender_auth: Some(MessageSenderAuth::VerifiedAllowlisted),
            },
            Some(MessageConversation {
                stable_id: delivery.conversation_id,
                display_name: None,
                alias: None,
            }),
            delivery.text,
        );
        report.extension_data = report_id.extension_data();
        let mut pending = GatewayPendingDelivery {
            session_id: delivery.session_id,
            agent_id,
            message_id,
            publisher_name,
            canonical_echo_observed: false,
        };
        if let Some(previous) = state_guard.gateway_pending_deliveries.get(&report_id)
            && previous.session_id == pending.session_id
            && previous.agent_id == pending.agent_id
            && previous.message_id == pending.message_id
            && previous.publisher_name == pending.publisher_name
        {
            pending.canonical_echo_observed = previous.canonical_echo_observed;
        }
        state_guard
            .gateway_pending_deliveries
            .insert(report_id, pending);
        drop(gateway_guard);
        drop(state_guard);
        if output
            .emit_message_report(Event::MessageDeliveredReported(report))
            .is_err()
        {
            return ProcessingControl::Stop;
        }
    }
    ProcessingControl::Continue
}

/// Retry every validated canonical ACK against the current gateway connection.
///
/// Returns false when the connection must be retired and replaced.
fn retry_gateway_acknowledgements(
    state: &SharedState,
    output: &Output,
    gateway_cell: &Mutex<Option<Arc<GatewayClient>>>,
    gateway: &Arc<GatewayClient>,
) -> bool {
    let mut pending = state
        .lock()
        .gateway_pending_deliveries
        .iter()
        .filter(|(_, pending)| pending.canonical_echo_observed)
        .map(|(report_id, pending)| {
            (
                report_id.clone(),
                pending.session_id.clone(),
                pending.agent_id.clone(),
            )
        })
        .collect::<Vec<_>>();
    pending.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));
    for (report_id, session_id, agent_id) in pending {
        let response = match gateway.acknowledge_delivery(
            report_id.as_str(),
            &session_id,
            agent_id.as_ref(),
        ) {
            Ok(response) => response,
            Err(error) if error.is_connection_fatal() => {
                fail_gateway_client_if_current(gateway_cell, state, gateway);
                return false;
            }
            Err(_) => return true,
        };
        if gateway_response_requires_reconnect(gateway, &response) {
            fail_gateway_client_if_current(gateway_cell, state, gateway);
            return false;
        }
        let mut state_guard = state.lock();
        let gateway_guard = gateway_cell.lock().expect("gateway lock");
        let is_current = gateway_guard
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, gateway));
        let still_matches = state_guard
            .gateway_pending_deliveries
            .get(&report_id)
            .is_some_and(|pending| {
                pending.canonical_echo_observed
                    && pending.session_id == session_id
                    && pending.agent_id == agent_id
            });
        if !is_current {
            return false;
        }
        if still_matches {
            state_guard.gateway_pending_deliveries.remove(&report_id);
        }
        drop(gateway_guard);
        drop(state_guard);
        if emit_gateway_deliveries(
            state,
            output,
            gateway_cell,
            Arc::clone(gateway),
            response.deliveries,
        ) == ProcessingControl::Stop
        {
            return false;
        }
    }
    true
}

/// Retire a failed gateway connection only when it remains current.
///
/// Desired local registrations survive so the supervisor can reannounce them
/// after reconnecting.
fn fail_gateway_client_if_current(
    gateway_cell: &Mutex<Option<Arc<GatewayClient>>>,
    state: &SharedState,
    failed_gateway: &Arc<GatewayClient>,
) -> bool {
    let is_current = {
        let mut slot = gateway_cell.lock().expect("gateway lock");
        if slot
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, failed_gateway))
        {
            *slot = None;
            true
        } else {
            false
        }
    };
    if !is_current {
        return false;
    }
    let mut state_guard = state.lock();
    state_guard.mark_coordination_changed();
    drop(state_guard);
    state.notify_all();
    true
}

/// Shared Telegram state plus a condition variable for waking the poller.
struct SharedState {
    /// Mutable extension state guarded by a single process-local mutex.
    state: Mutex<State>,
    /// Wakes local poller waits after configuration, registration, or shutdown.
    changed: Condvar,
    /// Test-only one-shot signal emitted while holding state immediately before
    /// a coordination wait atomically releases that lock.
    #[cfg(test)]
    wait_observer: Mutex<Option<mpsc::Sender<()>>>,
    /// Test-only one-shot signal emitted while holding state immediately before
    /// the poller atomically waits for registration or shutdown.
    #[cfg(test)]
    readiness_wait_observer: Mutex<Option<mpsc::Sender<()>>>,
}

impl SharedState {
    /// Create an empty shared state cell.
    fn new() -> Self {
        Self {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            #[cfg(test)]
            wait_observer: Mutex::new(None),
            #[cfg(test)]
            readiness_wait_observer: Mutex::new(None),
        }
    }

    /// Lock the shared state, recovering from panics in tests or callbacks.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Wait while `condition` remains true, recovering a poisoned mutex.
    fn wait_while<'a, F>(&self, guard: MutexGuard<'a, State>, condition: F) -> MutexGuard<'a, State>
    where
        F: FnMut(&mut State) -> bool,
    {
        self.changed
            .wait_while(guard, condition)
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Wait for a bounded delay while `condition` remains true.
    fn wait_timeout_while<'a, F>(
        &self,
        guard: MutexGuard<'a, State>,
        delay: Duration,
        condition: F,
    ) -> MutexGuard<'a, State>
    where
        F: FnMut(&mut State) -> bool,
    {
        let (guard, _timeout) = self
            .changed
            .wait_timeout_while(guard, delay, condition)
            .unwrap_or_else(|e| e.into_inner());
        guard
    }

    /// Wake all current state waiters.
    fn notify_all(&self) {
        self.changed.notify_all();
    }

    /// Install a one-shot deterministic observer for the next coordination
    /// wait.
    #[cfg(test)]
    fn observe_next_wait(&self, observer: mpsc::Sender<()>) {
        *self.wait_observer.lock().expect("wait observer lock") = Some(observer);
    }

    /// Signal a test while the caller still holds the state lock immediately
    /// before entering the condition-variable wait.
    #[cfg(test)]
    fn notify_wait_observer(&self) {
        if let Some(observer) = self
            .wait_observer
            .lock()
            .expect("wait observer lock")
            .take()
        {
            let _ = observer.send(());
        }
    }

    /// Install a one-shot deterministic observer for the next poller readiness
    /// wait.
    #[cfg(test)]
    fn observe_next_readiness_wait(&self, observer: mpsc::Sender<()>) {
        *self
            .readiness_wait_observer
            .lock()
            .expect("readiness wait observer lock") = Some(observer);
    }

    /// Signal a test while the poller still holds state immediately before
    /// entering its readiness condition-variable wait.
    #[cfg(test)]
    fn notify_readiness_wait_observer(&self) {
        if let Some(observer) = self
            .readiness_wait_observer
            .lock()
            .expect("readiness wait observer lock")
            .take()
        {
            let _ = observer.send(());
        }
    }
}

impl State {
    /// Record a change to config, registration, or shutdown that affects waits.
    fn mark_coordination_changed(&mut self) {
        self.coordination_generation = self.coordination_generation.wrapping_next();
    }

    /// Return whether registered or registering agents still need ownership of
    /// the local Telegram update stream.
    fn has_update_stream_interest(&self) -> bool {
        !self.registered_agents.is_empty() || self.pending_local_registrations != 0
    }

    /// Release update-stream ownership after all registered and registering
    /// agents have gone away.
    fn retire_update_stream_lock_if_idle(&mut self) {
        if !self.has_update_stream_interest() {
            self.update_stream_lock = None;
        }
    }

    /// Acquire the advisory stream lock unless this state already holds it.
    fn ensure_update_stream_locked(&mut self, cfg: &RuntimeConfig) -> Result<(), String> {
        if self
            .update_stream_lock
            .as_ref()
            .is_some_and(|lock| lock.covers(cfg.stream_identity()))
        {
            return Ok(());
        }
        let state_dir = self.state_dir.as_deref().ok_or_else(|| {
            "telegram update polling requires an extension state directory for advisory locking"
                .to_owned()
        })?;
        self.update_stream_lock = Some(Arc::new(
            UpdateStreamLock::acquire(state_dir, cfg.stream_identity())
                .map_err(|error| error.to_string())?,
        ));
        Ok(())
    }

    /// Clear active runtime bridge state after config loss or fail-closed
    /// errors.
    fn clear_active_bridge_state(&mut self) {
        self.config = None;
        self.gateway_config_generation = None;
        self.update_stream_lock = None;
        self.registered_agents.clear();
        self.selected_agent_by_chat.clear();
        self.learned_chat = None;
        self.poller_drained_initial_backlog = false;
        self.next_update_offset = None;
        self.live_checkpoints.clear();
        self.gateway_pending_deliveries.clear();
    }
}

enum ToolDispatch {
    Terminal(Box<Event>),
    Indeterminate(String),
}

impl ToolDispatch {
    fn terminal(event: Event) -> Self {
        Self::Terminal(Box::new(event))
    }
}

struct Extension {
    state: Arc<SharedState>,
    client: Arc<dyn TelegramClient>,
    gateway: Arc<Mutex<Option<Arc<GatewayClient>>>>,
    /// Sole connect/reconnect worker for the active gateway configuration.
    gateway_supervisor: GatewaySupervisor,
    /// Serializes configuration replacement and supervisor ownership transfer.
    config_apply: Mutex<()>,
    output: Output,
    shutdown: Arc<AtomicBool>,
    tool_names: ToolNames,
    /// Owned local poller, joined only for forced mandatory-output teardown.
    poller_handle: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Orders shutdown classification against poller report publication.
    poller_publication: Arc<Mutex<()>>,
}

impl Extension {
    fn new(client: Arc<dyn TelegramClient>, output: impl Into<Output>) -> Self {
        Self::with_tool_names(client, output, ToolNames::logical())
    }

    fn with_tool_names(
        client: Arc<dyn TelegramClient>,
        output: impl Into<Output>,
        tool_names: ToolNames,
    ) -> Self {
        Self {
            state: Arc::new(SharedState::new()),
            client,
            gateway: Arc::new(Mutex::new(None)),
            gateway_supervisor: GatewaySupervisor::new(),
            config_apply: Mutex::new(()),
            output: output.into(),
            shutdown: Arc::new(AtomicBool::new(false)),
            tool_names,
            poller_handle: Mutex::new(None),
            poller_publication: Arc::new(Mutex::new(())),
        }
    }

    fn apply_config(
        &self,
        mode: impl Into<BridgeMode>,
        state_dir: Option<std::path::PathBuf>,
    ) -> Result<(), String> {
        let _apply_guard = self.config_apply.lock().expect("config apply lock");
        let result = match mode.into() {
            BridgeMode::LocalPoll(cfg) => self.apply_local_poll_config(cfg, state_dir),
            BridgeMode::GatewayClient(cfg) => self.apply_gateway_client_config(cfg, state_dir),
        };
        if result.is_ok() {
            tracing::info!(target: LOG_TARGET, "telegram configured");
        }
        result
    }

    fn set_publisher_name(&self, publisher_name: tau_proto::ExtensionName) {
        self.state.lock().publisher_name = Some(publisher_name);
    }

    /// Loads one session's strict desired-registration snapshot.
    fn load_desired_registrations(
        &self,
        storage: &DesiredRegistrationStorage,
        session_id: &tau_proto::SessionId,
    ) -> Result<(), String> {
        let desired = storage.load(session_id)?;
        let mut state = self.state.lock();
        state.desired_registrations = desired;
        state.replayed_loaded_agents.clear();
        Ok(())
    }

    /// Durably changes one desired registration before committing in-memory
    /// intent.
    fn persist_desired_registration(
        &self,
        storage: &DesiredRegistrationStorage,
        session_id: &tau_proto::SessionId,
        agent_id: &AgentId,
        enabled: bool,
    ) -> Result<(), DesiredRegistrationStoreError> {
        let mut replacement = self.state.lock().desired_registrations.clone();
        if replacement.contains(agent_id) == enabled {
            return Ok(());
        }
        if enabled {
            replacement.insert(agent_id.clone());
        } else {
            replacement.remove(agent_id);
        }
        storage.store(session_id, &replacement)?;
        if self.state.lock().current_session_id.as_ref() != Some(session_id) {
            return Err(DesiredRegistrationStoreError::Known(
                "Telegram registration session changed during persistence".to_owned(),
            ));
        }
        self.state.lock().desired_registrations = replacement;
        Ok(())
    }

    /// Converts one typed storage failure without erasing fail-stop authority.
    fn desired_registration_dispatch(
        invoke: ToolStarted,
        error: DesiredRegistrationStoreError,
    ) -> ToolDispatch {
        match error {
            DesiredRegistrationStoreError::Known(message) => {
                ToolDispatch::terminal(tool_error(invoke, message))
            }
            DesiredRegistrationStoreError::Indeterminate(message) => {
                ToolDispatch::Indeterminate(message)
            }
        }
    }

    /// Reconciles durable desire with the complete replayed loaded membership.
    fn reconcile_desired_registrations(
        &self,
        storage: &DesiredRegistrationStorage,
        session_id: &tau_proto::SessionId,
    ) -> Result<(), String> {
        let (desired, loaded, gateway_mode) = {
            let state = self.state.lock();
            (
                state.desired_registrations.clone(),
                state.replayed_loaded_agents.clone(),
                state.gateway_config_generation.is_some(),
            )
        };
        let loaded = loaded.into_iter().collect::<BTreeSet<_>>();
        let reconciled = desired
            .intersection(&loaded)
            .cloned()
            .collect::<BTreeSet<_>>();
        if reconciled != desired {
            storage
                .store(session_id, &reconciled)
                .map_err(|error| error.to_string())?;
            self.state.lock().desired_registrations = reconciled.clone();
        }
        if gateway_mode {
            let mut state = self.state.lock();
            state.registered_agents.extend(reconciled);
            state.registration_replay_complete = true;
            state.mark_coordination_changed();
            drop(state);
            self.state.notify_all();
            if let Some(gateway) = self.gateway_client() {
                fail_gateway_client_if_current(&self.gateway, &self.state, &gateway);
                gateway.disconnect();
            }
            return Ok(());
        }
        self.state.lock().registration_replay_complete = true;
        for agent_id in reconciled {
            self.activate_local_registration(&agent_id)?;
        }
        Ok(())
    }

    fn publisher_claim(&self) -> RawMessagePublisherId {
        RawMessagePublisherId::new(
            self.state
                .lock()
                .publisher_name
                .as_ref()
                .expect("configured Telegram runtime retains its instance name")
                .as_str(),
        )
    }

    /// Advance local polling only when this instance receives the exact
    /// canonical echo for a retained routed report.
    fn acknowledge_live_delivery(&self, fact: &MessageDelivered) {
        let mut state = self.state.lock();
        let publisher_matches = state
            .publisher_name
            .as_ref()
            .is_some_and(|publisher| publisher.as_str() == fact.publisher_extension_id.as_str());
        let acknowledged = state
            .live_checkpoints
            .acknowledge_canonical(publisher_matches, fact);
        if !acknowledged {
            let Some(report_id) = TelegramReportId::from_extension_data(&fact.extension_data)
            else {
                return;
            };
            let Some(pending) = state.gateway_pending_deliveries.get_mut(&report_id) else {
                return;
            };
            if pending.publisher_name.as_str() != fact.publisher_extension_id.as_str()
                || pending.agent_id.as_ref() != fact.agent_id.as_str()
                || pending.message_id != fact.message_id
            {
                return;
            }
            pending.canonical_echo_observed = true;
            drop(state);
            if let Some(gateway) = self.gateway_client() {
                retry_gateway_acknowledgements(&self.state, &self.output, &self.gateway, &gateway);
            }
            return;
        }
        let next_update_offset = state.next_update_offset;
        state.next_update_offset = state
            .live_checkpoints
            .advance_acknowledged_prefix(next_update_offset);
        state.mark_coordination_changed();
        drop(state);
        self.state.notify_all();
    }

    fn apply_local_poll_config(
        &self,
        cfg: RuntimeConfig,
        state_dir: Option<std::path::PathBuf>,
    ) -> Result<(), String> {
        let mut state = self.state.lock();
        let switched_from_gateway = state.gateway_config_generation.take().is_some();
        state.config_generation = state.config_generation.next();
        if switched_from_gateway {
            state.clear_active_bridge_state();
        }
        state.state_dir = state_dir;
        let update_stream_changed = state
            .config
            .as_ref()
            .is_some_and(|old_cfg| !old_cfg.uses_same_update_stream_as(&cfg));
        let old_active_chat = state
            .config
            .as_ref()
            .and_then(|cfg| cfg.configured_chat_id)
            .or_else(|| state.learned_chat.map(|chat| chat.chat_id));
        let learned_chat_is_stale = cfg.configured_chat_id.is_some()
            || state
                .learned_chat
                .is_some_and(|chat| !cfg.allowed_user_ids.contains(&chat.user_id));
        if learned_chat_is_stale {
            state.learned_chat = None;
        }
        let new_active_chat = cfg
            .configured_chat_id
            .or_else(|| state.learned_chat.map(|chat| chat.chat_id));
        if old_active_chat != new_active_chat {
            state.registered_agents.clear();
            state.selected_agent_by_chat.clear();
        }
        if update_stream_changed {
            state.poller_drained_initial_backlog = false;
            state.next_update_offset = None;
            state.live_checkpoints.clear();
            state.update_stream_lock = None;
        }
        if !state.registered_agents.is_empty()
            && let Err(message) = state.ensure_update_stream_locked(&cfg)
        {
            state.clear_active_bridge_state();
            state.mark_coordination_changed();
            self.state.notify_all();
            return Err(message);
        }
        state.config = Some(cfg);
        state.mark_coordination_changed();
        self.state.notify_all();
        drop(state);
        self.stop_gateway_supervisor();
        Ok(())
    }

    fn apply_gateway_client_config(
        &self,
        cfg: GatewayClientConfig,
        _state_dir: Option<std::path::PathBuf>,
    ) -> Result<(), String> {
        let config_generation = {
            let mut state = self.state.lock();
            state.config_generation = state.config_generation.next();
            state.clear_active_bridge_state();
            state.gateway_config_generation = Some(state.config_generation);
            state.mark_coordination_changed();
            self.state.notify_all();
            state.config_generation
        };
        self.stop_gateway_supervisor();
        self.gateway_supervisor.start(
            Arc::clone(&self.state),
            self.output.clone(),
            Arc::clone(&self.shutdown),
            Arc::clone(&self.gateway),
            config_generation,
            cfg,
        )
    }

    fn clear_config_after_error(&self) {
        let _apply_guard = self.config_apply.lock().expect("config apply lock");
        let mut state = self.state.lock();
        state.config_generation = state.config_generation.next();
        state.clear_active_bridge_state();
        state.gateway_config_generation = None;
        state.mark_coordination_changed();
        self.state.notify_all();
        drop(state);
        self.stop_gateway_supervisor();
    }

    fn gateway_client(&self) -> Option<Arc<GatewayClient>> {
        self.gateway.lock().expect("gateway lock").clone()
    }

    /// Return whether gateway-client mode has an active supervisor
    /// configuration, even while its socket is disconnected.
    fn gateway_mode_configured(&self) -> bool {
        self.state.lock().gateway_config_generation.is_some()
    }

    /// Stop and join the previous bounded gateway supervisor.
    fn stop_gateway_supervisor(&self) {
        self.gateway_supervisor.stop(&self.state, &self.gateway);
    }

    /// Accept a response only from the current connection and generation.
    fn apply_gateway_response(
        &self,
        gateway: &Arc<GatewayClient>,
        response: GatewaySocketResponse,
    ) -> bool {
        if gateway_response_requires_reconnect(gateway, &response) {
            fail_gateway_client_if_current(&self.gateway, &self.state, gateway);
            gateway.disconnect();
            return false;
        }
        if !self
            .gateway
            .lock()
            .expect("gateway lock")
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, gateway))
        {
            return false;
        }
        if emit_gateway_deliveries(
            &self.state,
            &self.output,
            &self.gateway,
            Arc::clone(gateway),
            response.deliveries,
        ) == ProcessingControl::Stop
        {
            return false;
        }
        true
    }

    fn request_shutdown(&self) {
        let mut state = self.state.lock();
        state.shutdown_requested = true;
        state.mark_coordination_changed();
        self.shutdown.store(true, Ordering::Relaxed);
        self.state.notify_all();
        drop(state);
        self.stop_gateway_supervisor();
    }

    /// Signal prompt detached shutdown without joining workers that may be
    /// blocked in an already-admitted checked write.
    fn request_shutdown_detached(&self) {
        let mut state = self.state.lock();
        state.shutdown_requested = true;
        state.mark_coordination_changed();
        self.shutdown.store(true, Ordering::Relaxed);
        self.state.notify_all();
        if let Some(gateway) = self.gateway.lock().expect("gateway lock").as_ref() {
            gateway.disconnect();
        }
    }

    /// Settle a forced mandatory-output teardown after signaling shutdown.
    fn join_poller_after_output_failure(&self) {
        if let Some(handle) = self.poller_handle.lock().expect("poller handle").take() {
            let _ = handle.join();
        }
    }

    /// Signal shutdown without racing a poller that already passed its loop
    /// check but has not yet published its mandatory report.
    fn shutdown_after_runtime_error(&self) {
        let _publication = self
            .poller_publication
            .lock()
            .expect("poller publication gate");
        self.request_shutdown();
        let mandatory_failed = self.output.check_mandatory_output().is_err();
        drop(_publication);
        if mandatory_failed {
            self.join_poller_after_output_failure();
        }
    }

    fn poll_response_matches_config(&self, config_generation: ConfigGeneration) -> bool {
        let state = self.state.lock();
        state.config.is_some() && state.config_generation == config_generation
    }

    fn dispatch_tool_checked(
        &self,
        invoke: ToolStarted,
        storage: &DesiredRegistrationStorage,
    ) -> ClientResult<()> {
        self.output.report_tool_progress(ToolProgress {
            call_id: invoke.call_id.clone(),
            tool_name: invoke.tool_name.clone(),
            message: Some("telegram tool started".to_owned()),
            progress: None,
            display: Some(ToolUseState {
                status: ToolUseStatus::InProgress,
                status_text: tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
                ..Default::default()
            }),
        });
        let dispatch = match invoke.tool_name.as_str() {
            name if name == self.tool_names.register.as_str() => {
                self.handle_register(invoke, storage)
            }
            name if name == self.tool_names.send.as_str() => {
                ToolDispatch::terminal(self.handle_send(invoke))
            }
            _ => ToolDispatch::terminal(tool_error(invoke, "unknown telegram tool".to_owned())),
        };
        let event = match dispatch {
            ToolDispatch::Terminal(event) => *event,
            ToolDispatch::Indeterminate(message) => {
                self.output.report_known_mandatory_failure();
                return Err(tau_client::ClientError::handler(format!(
                    "desired Telegram registration storage outcome is indeterminate: {message}"
                )));
            }
        };
        self.output.check_mandatory_output()?;
        self.output.report_tool_terminal(event)
    }

    /// Dispatch one tool for direct unit tests whose channel output is
    /// infallible.
    #[cfg(test)]
    fn dispatch_tool(&self, invoke: ToolStarted) {
        self.dispatch_tool_checked(invoke, &DesiredRegistrationStorage::default())
            .expect("test output channel remains connected");
    }

    fn handles_tool(&self, tool_name: &str) -> bool {
        tool_name == self.tool_names.register.as_str() || tool_name == self.tool_names.send.as_str()
    }

    /// Activates one local-poll registration after all preflight checks.
    fn activate_local_registration(&self, agent_id: &AgentId) -> Result<(), String> {
        let mut state = self.state.lock();
        let was_unregistered = state.registered_agents.is_empty();
        let cfg = state
            .config
            .clone()
            .ok_or_else(|| "telegram extension is not configured".to_owned())?;
        if was_unregistered {
            state.ensure_update_stream_locked(&cfg)?;
            state.pending_local_registrations += 1;
            let config_generation = state.config_generation;
            drop(state);
            let webhook_result = self.check_webhook_allows_get_updates(&cfg, config_generation);
            state = self.state.lock();
            state.pending_local_registrations -= 1;
            if let Err(message) = webhook_result {
                state.mark_coordination_changed();
                self.state.notify_all();
                return Err(message);
            }
            if state.config_generation != config_generation
                || state
                    .config
                    .as_ref()
                    .is_none_or(|current| !current.uses_same_update_stream_as(&cfg))
            {
                state.mark_coordination_changed();
                self.state.notify_all();
                return Err(
                    "telegram configuration changed while checking webhook status".to_owned(),
                );
            }
            if !state
                .update_stream_lock
                .as_ref()
                .is_some_and(|lock| lock.covers(cfg.stream_identity()))
            {
                state.mark_coordination_changed();
                self.state.notify_all();
                return Err(
                    "telegram update-stream lock was lost while checking webhook status".to_owned(),
                );
            }
        } else if !state
            .update_stream_lock
            .as_ref()
            .is_some_and(|lock| lock.covers(cfg.stream_identity()))
        {
            return Err("telegram update-stream lock is not held by this registration".to_owned());
        }
        self.ensure_poller_started_locked(&mut state);
        state.registered_agents.insert(agent_id.clone());
        if was_unregistered {
            state.poller_drained_initial_backlog = false;
        }
        state
            .agent_labels
            .entry(agent_id.clone())
            .or_insert_with(|| agent_id.to_string());
        state.mark_coordination_changed();
        drop(state);
        self.state.notify_all();
        Ok(())
    }

    /// Revokes one process-local registration and its selection authority.
    fn revoke_local_registration(&self, agent_id: &AgentId) {
        let mut state = self.state.lock();
        state.registered_agents.remove(agent_id);
        state
            .selected_agent_by_chat
            .retain(|_, agent| agent != agent_id);
        if state.registered_agents.is_empty() {
            state.poller_drained_initial_backlog = false;
        }
        state.mark_coordination_changed();
        drop(state);
        self.state.notify_all();
    }

    fn handle_register(
        &self,
        invoke: ToolStarted,
        storage: &DesiredRegistrationStorage,
    ) -> ToolDispatch {
        if let Err(message) = validate_object_fields(&invoke.arguments, &["enabled"]) {
            return ToolDispatch::terminal(tool_error(invoke, message));
        }
        let enabled = match cbor_bool_field(&invoke.arguments, "enabled") {
            Ok(enabled) => enabled,
            Err(message) => return ToolDispatch::terminal(tool_error(invoke, message)),
        };
        let Some(session_id) = self.state.lock().current_session_id.clone() else {
            return ToolDispatch::terminal(tool_error(
                invoke,
                "telegram extension has not observed session.started yet".to_owned(),
            ));
        };
        if self.gateway_mode_configured() {
            return self.handle_gateway_register(invoke, enabled, storage, &session_id);
        }
        if enabled {
            if let Err(message) = self.activate_local_registration(&invoke.agent_id) {
                return ToolDispatch::terminal(tool_error(invoke, message));
            }
            if let Err(message) =
                self.persist_desired_registration(storage, &session_id, &invoke.agent_id, true)
            {
                self.revoke_local_registration(&invoke.agent_id);
                return Self::desired_registration_dispatch(invoke, message);
            }
        } else {
            if let Err(message) =
                self.persist_desired_registration(storage, &session_id, &invoke.agent_id, false)
            {
                return Self::desired_registration_dispatch(invoke, message);
            }
            self.revoke_local_registration(&invoke.agent_id);
        }
        ToolDispatch::terminal(tool_result(
            invoke,
            if enabled {
                "registered for Telegram messages"
            } else {
                "unregistered from Telegram messages"
            },
        ))
    }

    fn handle_gateway_register(
        &self,
        invoke: ToolStarted,
        enabled: bool,
        storage: &DesiredRegistrationStorage,
        expected_session_id: &tau_proto::SessionId,
    ) -> ToolDispatch {
        if !enabled
            && let Err(message) = self.persist_desired_registration(
                storage,
                expected_session_id,
                &invoke.agent_id,
                false,
            )
        {
            return Self::desired_registration_dispatch(invoke, message);
        }
        let (session_id, display_name, config_generation) = {
            let mut state = self.state.lock();
            let Some(session_id) = state.current_session_id.clone() else {
                return ToolDispatch::terminal(tool_error(
                    invoke,
                    "telegram gateway client has not observed session.started yet".to_owned(),
                ));
            };
            let display_name = state.agent_labels.get(&invoke.agent_id).cloned();
            let config_generation = state.config_generation;
            if !enabled {
                state.registered_agents.remove(&invoke.agent_id);
                state.mark_coordination_changed();
                self.state.notify_all();
            }
            (session_id, display_name, config_generation)
        };
        let Some(gateway) = self.gateway_client() else {
            return ToolDispatch::terminal(if enabled {
                tool_error(
                    invoke,
                    "telegram gateway is disconnected; registration failed closed".to_owned(),
                )
            } else {
                tool_result(
                    invoke,
                    "removed local Telegram gateway registration while disconnected",
                )
            });
        };
        let response = if enabled {
            gateway.register_agent(session_id.as_ref(), invoke.agent_id.as_ref(), display_name)
        } else {
            gateway.unregister_agent(session_id.as_ref(), invoke.agent_id.as_ref())
        };
        match response {
            Ok(response) => {
                if gateway_response_requires_reconnect(&gateway, &response) {
                    fail_gateway_client_if_current(&self.gateway, &self.state, &gateway);
                    gateway.disconnect();
                    return ToolDispatch::terminal(tool_error(
                        invoke,
                        "telegram gateway changed generation during registration".to_owned(),
                    ));
                }
                {
                    let mut state = self.state.lock();
                    let gateway_is_current = self
                        .gateway
                        .lock()
                        .expect("gateway lock")
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, &gateway));
                    if state.config_generation != config_generation || !gateway_is_current {
                        return ToolDispatch::terminal(tool_error(
                            invoke,
                            "telegram gateway configuration changed during registration".to_owned(),
                        ));
                    }
                    if enabled {
                        state.registered_agents.insert(invoke.agent_id.clone());
                    }
                    state.mark_coordination_changed();
                }
                self.state.notify_all();
                if enabled
                    && let Err(message) = self.persist_desired_registration(
                        storage,
                        expected_session_id,
                        &invoke.agent_id,
                        true,
                    )
                {
                    self.revoke_local_registration(&invoke.agent_id);
                    let rollback_authoritative = gateway
                        .unregister_agent(session_id.as_ref(), invoke.agent_id.as_ref())
                        .is_ok_and(|response| {
                            !gateway_response_requires_reconnect(&gateway, &response)
                        });
                    if !rollback_authoritative {
                        fail_gateway_client_if_current(&self.gateway, &self.state, &gateway);
                        gateway.disconnect();
                    }
                    return Self::desired_registration_dispatch(invoke, message);
                }
                if !self.apply_gateway_response(&gateway, response) {
                    return ToolDispatch::terminal(tool_error(
                        invoke,
                        "telegram gateway changed generation during registration".to_owned(),
                    ));
                }
                ToolDispatch::terminal(tool_result(
                    invoke,
                    if enabled {
                        "registered with Telegram gateway"
                    } else {
                        "unregistered from Telegram gateway"
                    },
                ))
            }
            Err(message) => {
                if message.is_connection_fatal() {
                    fail_gateway_client_if_current(&self.gateway, &self.state, &gateway);
                }
                ToolDispatch::terminal(tool_error(invoke, message.to_string()))
            }
        }
    }

    fn check_webhook_allows_get_updates(
        &self,
        cfg: &RuntimeConfig,
        config_generation: ConfigGeneration,
    ) -> Result<(), String> {
        match self.client.get_webhook_info(cfg) {
            Ok(info) if !info.url.trim().is_empty() => {
                let message = webhook_active_message(&info);
                self.release_update_stream_lock_if_current(cfg, config_generation);
                Err(message)
            }
            Ok(_) => Ok(()),
            Err(message) => {
                let message = format!(
                    "The Telegram registration tool could not verify Telegram webhook status before polling; \
                     registration was refused so Tau does not silently contend for the update \
                     stream: {message}"
                );
                self.release_update_stream_lock_if_current(cfg, config_generation);
                Err(message)
            }
        }
    }

    fn release_update_stream_lock_if_current(
        &self,
        cfg: &RuntimeConfig,
        config_generation: ConfigGeneration,
    ) {
        let mut state = self.state.lock();
        if state.config_generation == config_generation
            && state
                .config
                .as_ref()
                .is_some_and(|current| current.uses_same_update_stream_as(cfg))
        {
            state.update_stream_lock = None;
            state.mark_coordination_changed();
            self.state.notify_all();
        }
    }

    fn fail_active_polling_with_notice(&self, cfg: &RuntimeConfig, message: &str) {
        {
            let mut state = self.state.lock();
            if state
                .config
                .as_ref()
                .is_some_and(|current| current.uses_same_update_stream_as(cfg))
            {
                state.update_stream_lock = None;
                state.registered_agents.clear();
                state.selected_agent_by_chat.clear();
                state.poller_drained_initial_backlog = false;
                state.mark_coordination_changed();
                self.state.notify_all();
            }
        }
        self.report_telegram_polling_notice(message);
    }

    fn report_telegram_polling_notice(&self, message: &str) {
        self.output.request_notice(message, NoticeLevel::Warning);
    }

    fn ensure_poller_started_locked(&self, state: &mut State) {
        if state.poller_started {
            return;
        }
        state.poller_started = true;
        let state_arc = Arc::clone(&self.state);
        let output = self.output.clone();
        let client = Arc::clone(&self.client);
        let shutdown = Arc::clone(&self.shutdown);
        let tool_names = self.tool_names.clone();
        let poller_publication = Arc::clone(&self.poller_publication);
        let handle = std::thread::spawn(move || {
            poll_loop_with_tool_names(
                state_arc,
                client,
                output,
                shutdown,
                tool_names,
                poller_publication,
            );
        });
        *self.poller_handle.lock().expect("poller handle") = Some(handle);
    }

    fn handle_send(&self, invoke: ToolStarted) -> Event {
        if let Err(message) = validate_object_fields(&invoke.arguments, &["message"]) {
            return tool_error(invoke, message);
        }
        let message = match cbor_string_field(&invoke.arguments, "message") {
            Ok(message) => message,
            Err(message) => return tool_error(invoke, message),
        };
        if message.trim().is_empty() {
            return tool_error(invoke, "`message` must not be empty".to_owned());
        }
        if self.gateway_mode_configured() {
            let Some(gateway) = self.gateway_client() else {
                return tool_error(
                    invoke,
                    "telegram gateway is disconnected; send failed closed".to_owned(),
                );
            };
            if message.len() > MAX_GATEWAY_OUTBOUND_MESSAGE_BYTES {
                return tool_error(
                    invoke,
                    "`message` is too large for telegram gateway send".to_owned(),
                );
            }
            let session_id = {
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
                let Some(session_id) = state.current_session_id.clone() else {
                    return tool_error(
                        invoke,
                        "telegram gateway client has not observed session.started yet".to_owned(),
                    );
                };
                session_id
            };
            match gateway.send_message(session_id.as_ref(), invoke.agent_id.as_ref(), &message) {
                Ok(response) => {
                    if !self.apply_gateway_response(&gateway, response) {
                        return tool_error(
                            invoke,
                            "telegram gateway disconnected during send".to_owned(),
                        );
                    }
                    self.emit_sent_report(
                        &invoke.agent_id,
                        invoke.call_id.as_str(),
                        None,
                        &message,
                    );
                    tool_result(invoke, "sent Telegram message through gateway")
                }
                Err(message) => {
                    if message.is_connection_fatal() {
                        fail_gateway_client_if_current(&self.gateway, &self.state, &gateway);
                    }
                    tool_error(invoke, message.to_string())
                }
            }
        } else {
            let (cfg, chat_id) = {
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
                    return tool_error(invoke, "telegram extension is not configured".to_owned());
                };
                let Some(chat_id) = cfg
                    .configured_chat_id
                    .or_else(|| state.learned_chat.map(|chat| chat.chat_id))
                else {
                    return tool_error(
                        invoke,
                        "telegram chat is not linked; send /start to the bot or configure chat_id"
                            .to_owned(),
                    );
                };
                (cfg, chat_id)
            };
            let text = format!("[{}] {message}", invoke.agent_id.as_ref());
            match self.client.send_message(&cfg, chat_id, &text) {
                Ok(()) => {
                    self.emit_sent_report(
                        &invoke.agent_id,
                        invoke.call_id.as_str(),
                        Some(chat_id),
                        &message,
                    );
                    tool_result(invoke, "sent Telegram message")
                }
                Err(message) => tool_error(invoke, message.to_string()),
            }
        }
    }

    fn process_update_for_generation(
        &self,
        update: TgUpdate,
        config_generation: ConfigGeneration,
    ) -> ProcessingControl {
        let update_id = update.update_id;
        let existing = {
            let state = self.state.lock();
            if state.config_generation != config_generation || state.config.is_none() {
                return ProcessingControl::Continue;
            }
            state
                .live_checkpoints
                .existing_update(update_id, state.next_update_offset)
        };
        match existing {
            ExistingUpdate::Acknowledged => return ProcessingControl::Continue,
            ExistingUpdate::Routed(report) => {
                return if self.output.emit_message_report(*report).is_ok() {
                    ProcessingControl::Continue
                } else {
                    ProcessingControl::Stop
                };
            }
            ExistingUpdate::NonRouted => {
                let _ = self.classify_update_for_generation(update, config_generation);
                return ProcessingControl::Continue;
            }
            ExistingUpdate::New => {}
        }

        let disposition = self.classify_update_for_generation(update, config_generation);
        let report = {
            let mut state = self.state.lock();
            if state.config_generation != config_generation || state.config.is_none() {
                return ProcessingControl::Continue;
            }
            match disposition {
                UpdateDisposition::Routed(route) => {
                    let report = route.report();
                    state.live_checkpoints.insert_routed(update_id, route);
                    Some(*report)
                }
                UpdateDisposition::NonRouted => {
                    state.live_checkpoints.insert_non_routed(update_id);
                    let next_update_offset = state.next_update_offset;
                    state.next_update_offset = state
                        .live_checkpoints
                        .advance_acknowledged_prefix(next_update_offset);
                    None
                }
            }
        };
        if let Some(report) = report {
            if self.output.emit_message_report(report).is_ok() {
                ProcessingControl::Continue
            } else {
                ProcessingControl::Stop
            }
        } else {
            ProcessingControl::Continue
        }
    }

    /// Process a provider batch only through the first failed mandatory report.
    fn process_update_batch(
        &self,
        updates: Vec<TgUpdate>,
        config_generation: ConfigGeneration,
    ) -> ProcessingControl {
        if updates.into_iter().all(|update| {
            self.process_update_for_generation(update, config_generation)
                == ProcessingControl::Continue
        }) {
            ProcessingControl::Continue
        } else {
            ProcessingControl::Stop
        }
    }

    /// During backlog drain, replay retained work but classify unseen stale
    /// updates as non-routed without Telegram replies or Tau reports.
    fn process_draining_update_for_generation(
        &self,
        update: TgUpdate,
        config_generation: ConfigGeneration,
    ) -> ProcessingControl {
        let update_id = update.update_id;
        let existing = {
            let state = self.state.lock();
            if state.config_generation != config_generation || state.config.is_none() {
                return ProcessingControl::Continue;
            }
            state
                .live_checkpoints
                .existing_update(update_id, state.next_update_offset)
        };
        match existing {
            ExistingUpdate::Acknowledged => {}
            ExistingUpdate::Routed(report) => {
                return if self.output.emit_message_report(*report).is_ok() {
                    ProcessingControl::Continue
                } else {
                    ProcessingControl::Stop
                };
            }
            ExistingUpdate::NonRouted => {
                let _ = self.classify_update_for_generation(update, config_generation);
            }
            ExistingUpdate::New => {
                let mut state = self.state.lock();
                if state.config_generation != config_generation || state.config.is_none() {
                    return ProcessingControl::Continue;
                }
                state.live_checkpoints.insert_non_routed(update_id);
                let next_update_offset = state.next_update_offset;
                state.next_update_offset = state
                    .live_checkpoints
                    .advance_acknowledged_prefix(next_update_offset);
            }
        }
        ProcessingControl::Continue
    }

    /// Drain retained work only through the first failed mandatory replay.
    fn process_draining_batch(
        &self,
        updates: Vec<TgUpdate>,
        config_generation: ConfigGeneration,
    ) -> ProcessingControl {
        if updates.into_iter().all(|update| {
            self.process_draining_update_for_generation(update, config_generation)
                == ProcessingControl::Continue
        }) {
            ProcessingControl::Continue
        } else {
            ProcessingControl::Stop
        }
    }

    /// Classify and perform local processing for one previously unseen update.
    fn classify_update_for_generation(
        &self,
        update: TgUpdate,
        config_generation: ConfigGeneration,
    ) -> UpdateDisposition {
        let update_id = update.update_id;
        let Some(message) = update.message else {
            return UpdateDisposition::NonRouted;
        };
        let Some(cfg) = self.config_for_allowed_message(&message, config_generation) else {
            return UpdateDisposition::NonRouted;
        };
        let is_private_chat = is_private_message_chat(&message);
        let active_chat = self.active_chat(&cfg);
        if self.rejects_inactive_chat(
            &cfg,
            &message,
            active_chat,
            is_private_chat,
            config_generation,
        ) {
            return UpdateDisposition::NonRouted;
        }
        let Some(text) = self.trimmed_message_text(&cfg, &message, config_generation) else {
            return UpdateDisposition::NonRouted;
        };
        let (command, rest) = parse_command(&text);
        if self.rejects_unlinked_command(&cfg, &message, active_chat, command, config_generation) {
            return UpdateDisposition::NonRouted;
        }
        if let Some(disposition) =
            self.handle_command(&cfg, &message, update_id, command, rest, config_generation)
        {
            return disposition;
        }

        self.route_plain_text(&cfg, &message, update_id, &text, config_generation)
    }

    fn config_for_allowed_message(
        &self,
        message: &TgMessage,
        config_generation: ConfigGeneration,
    ) -> Option<RuntimeConfig> {
        let state = self.state.lock();
        if state.config_generation != config_generation {
            return None;
        }
        let cfg = state.config.clone()?;
        if cfg.allowed_user_ids.contains(&message.user_id) {
            Some(cfg)
        } else {
            tracing::warn!(target: LOG_TARGET, "ignoring Telegram message from unallowed user");
            None
        }
    }

    fn active_chat(&self, cfg: &RuntimeConfig) -> Option<i64> {
        let state = self.state.lock();
        cfg.configured_chat_id
            .or_else(|| state.learned_chat.map(|chat| chat.chat_id))
    }

    fn rejects_inactive_chat(
        &self,
        cfg: &RuntimeConfig,
        message: &TgMessage,
        active_chat: Option<i64>,
        is_private_chat: bool,
        config_generation: ConfigGeneration,
    ) -> bool {
        if let Some(configured_chat_id) = cfg.configured_chat_id {
            if message.chat_id != configured_chat_id {
                self.reply(
                    cfg,
                    message.chat_id,
                    "This Tau bridge is configured for a different Telegram chat.",
                    config_generation,
                );
                return true;
            }
        } else if active_chat.is_some_and(|chat_id| chat_id != message.chat_id) {
            self.reply(
                cfg,
                message.chat_id,
                "This Tau bridge is already linked to a different Telegram chat.",
                config_generation,
            );
            return true;
        }

        if !is_private_chat && cfg.configured_chat_id != Some(message.chat_id) {
            self.reply(
                cfg,
                message.chat_id,
                "Group chats are only supported when this chat_id is explicitly configured.",
                config_generation,
            );
            return true;
        }
        false
    }

    fn trimmed_message_text(
        &self,
        cfg: &RuntimeConfig,
        message: &TgMessage,
        config_generation: ConfigGeneration,
    ) -> Option<String> {
        let text = message.text.as_deref().unwrap_or_default().trim();
        if text.is_empty() {
            self.reply(
                cfg,
                message.chat_id,
                "Only text messages are supported by this Tau bridge.",
                config_generation,
            );
            None
        } else {
            Some(text.to_owned())
        }
    }

    fn rejects_unlinked_command(
        &self,
        cfg: &RuntimeConfig,
        message: &TgMessage,
        active_chat: Option<i64>,
        command: Option<&str>,
        config_generation: ConfigGeneration,
    ) -> bool {
        if cfg.configured_chat_id.is_some()
            || active_chat.is_some()
            || matches!(command, Some("/start"))
        {
            return false;
        }

        self.reply(
            cfg,
            message.chat_id,
            "Send /start to link this private chat before routing messages to Tau.",
            config_generation,
        );
        true
    }

    fn handle_command(
        &self,
        cfg: &RuntimeConfig,
        message: &TgMessage,
        update_id: TelegramUpdateId,
        command: Option<&str>,
        rest: &str,
        config_generation: ConfigGeneration,
    ) -> Option<UpdateDisposition> {
        match command {
            Some("/start") => {
                self.handle_start_command(
                    cfg,
                    message,
                    is_private_message_chat(message),
                    config_generation,
                );
                Some(UpdateDisposition::NonRouted)
            }
            Some("/agents") => {
                self.handle_agents_command(cfg, message.chat_id, config_generation);
                Some(UpdateDisposition::NonRouted)
            }
            Some("/select") => {
                self.handle_select_command(cfg, message.chat_id, rest, config_generation);
                Some(UpdateDisposition::NonRouted)
            }
            Some("/to") => {
                Some(self.handle_to_command(cfg, message, update_id, rest, config_generation))
            }
            Some(_) => {
                self.reply(
                    cfg,
                    message.chat_id,
                    "Unknown Telegram command. Supported commands: /start, /agents, /select, /to.",
                    config_generation,
                );
                Some(UpdateDisposition::NonRouted)
            }
            None => None,
        }
    }

    fn handle_start_command(
        &self,
        cfg: &RuntimeConfig,
        message: &TgMessage,
        is_private_chat: bool,
        config_generation: ConfigGeneration,
    ) {
        if cfg.configured_chat_id.is_none() {
            if !is_private_chat {
                self.reply(
                    cfg,
                    message.chat_id,
                    "Group chats require an explicit configured chat_id.",
                    config_generation,
                );
                return;
            }
            let mut state = self.state.lock();
            if state.config_generation != config_generation {
                return;
            }
            state.learned_chat = Some(LinkedChat {
                chat_id: message.chat_id,
                user_id: message.user_id,
            });
        }
        self.reply(cfg, message.chat_id, help_text(), config_generation);
    }

    fn handle_agents_command(
        &self,
        cfg: &RuntimeConfig,
        chat_id: i64,
        config_generation: ConfigGeneration,
    ) {
        let reply = {
            let state = self.state.lock();
            if state.config_generation != config_generation {
                return;
            }
            agents_text(&state)
        };
        self.reply(cfg, chat_id, &reply, config_generation);
    }

    fn handle_select_command(
        &self,
        cfg: &RuntimeConfig,
        chat_id: i64,
        rest: &str,
        config_generation: ConfigGeneration,
    ) {
        if rest.trim().is_empty() {
            self.reply(
                cfg,
                chat_id,
                "Usage: /select <agent-id-or-prefix>",
                config_generation,
            );
            return;
        }

        let reply = {
            let mut state = self.state.lock();
            if state.config_generation != config_generation {
                return;
            }
            match resolve_agent(&state, rest.trim()) {
                Ok(agent_id) => {
                    state
                        .selected_agent_by_chat
                        .insert(chat_id, agent_id.clone());
                    format!("Selected {}", agent_designator(&state, &agent_id))
                }
                Err(reply) => reply,
            }
        };
        self.reply(cfg, chat_id, &reply, config_generation);
    }

    fn handle_to_command(
        &self,
        cfg: &RuntimeConfig,
        message: &TgMessage,
        update_id: TelegramUpdateId,
        rest: &str,
        config_generation: ConfigGeneration,
    ) -> UpdateDisposition {
        let (target, body) = split_first(rest);
        if target.is_empty() || body.trim().is_empty() {
            self.reply(
                cfg,
                message.chat_id,
                "Usage: /to <agent-id-or-prefix> <message>",
                config_generation,
            );
            return UpdateDisposition::NonRouted;
        }

        match self.resolve_registered_agent(target) {
            Ok(agent_id) => self.route_text(
                cfg,
                message,
                update_id,
                agent_id,
                body.trim(),
                config_generation,
            ),
            Err(reply) => {
                self.reply(cfg, message.chat_id, &reply, config_generation);
                UpdateDisposition::NonRouted
            }
        }
    }

    fn route_plain_text(
        &self,
        cfg: &RuntimeConfig,
        message: &TgMessage,
        update_id: TelegramUpdateId,
        text: &str,
        config_generation: ConfigGeneration,
    ) -> UpdateDisposition {
        match self.plain_text_target(message.chat_id) {
            Ok(agent_id) => {
                self.route_text(cfg, message, update_id, agent_id, text, config_generation)
            }
            Err(reply) => {
                self.reply(cfg, message.chat_id, &reply, config_generation);
                UpdateDisposition::NonRouted
            }
        }
    }

    fn plain_text_target(&self, chat_id: i64) -> Result<AgentId, String> {
        let state = self.state.lock();
        if let Some(agent_id) = state.selected_agent_by_chat.get(&chat_id)
            && state.registered_agents.contains(agent_id)
        {
            Ok(agent_id.clone())
        } else if state.registered_agents.len() == 1 {
            Ok(state
                .registered_agents
                .iter()
                .next()
                .expect("one agent")
                .clone())
        } else if state.registered_agents.is_empty() {
            Err(format!(
                "No Tau agents are registered. Ask an agent to call {}(enabled: true).",
                self.tool_names.register
            ))
        } else {
            Err(
                "Multiple Tau agents are registered. Use /agents then /select <agent-id-or-prefix>."
                    .to_owned(),
            )
        }
    }

    fn resolve_registered_agent(&self, target: &str) -> Result<AgentId, String> {
        let state = self.state.lock();
        resolve_agent(&state, target)
    }

    fn reply(
        &self,
        cfg: &RuntimeConfig,
        chat_id: i64,
        text: &str,
        config_generation: ConfigGeneration,
    ) {
        if !self.poll_response_matches_config(config_generation) {
            return;
        }
        let _ = self.client.send_message(cfg, chat_id, text);
    }

    fn route_text(
        &self,
        cfg: &RuntimeConfig,
        message: &TgMessage,
        update_id: TelegramUpdateId,
        agent_id: AgentId,
        text: &str,
        config_generation: ConfigGeneration,
    ) -> UpdateDisposition {
        if !self.poll_response_matches_config(config_generation) {
            return UpdateDisposition::NonRouted;
        }
        let message_id = telegram_message_ref(
            &message.chat_id.to_string(),
            &update_id.as_i64().to_string(),
        );
        let delivered = MessageDelivered::new(
            self.publisher_claim(),
            MessageAgentTarget::new(agent_id.as_ref()),
            message_id.clone(),
            MessageParty {
                stable_id: telegram_sender_ref(&message.user_id.to_string()),
                display_name: message.from_name.as_deref().and_then(bounded_display_name),
                sender_auth: Some(MessageSenderAuth::VerifiedAllowlisted),
            },
            Some(MessageConversation {
                stable_id: message.chat_id.to_string(),
                display_name: None,
                alias: None,
            }),
            text,
        );
        UpdateDisposition::Routed(RoutedUpdate::new(cfg, update_id, delivered))
    }

    /// Submit a remote Telegram send-success report before returning the
    /// terminal tool result through the same serialized extension writer.
    fn emit_sent_report(
        &self,
        agent_id: &AgentId,
        call_id: &str,
        chat_id: Option<i64>,
        text: &str,
    ) {
        let destination = chat_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "gateway".to_owned());
        let _ = self
            .output
            .emit_message_report(Event::MessageSentReported(MessageSent::new(
                self.publisher_claim(),
                MessageAgentTarget::new(agent_id.as_ref()),
                generated_send_message_id(call_id, &destination),
                None,
                chat_id.map(|id| MessageConversation {
                    stable_id: id.to_string(),
                    display_name: None,
                    alias: None,
                }),
                text,
            )));
    }
}

/// Derive a bounded publisher-unique send identity from the harness-unique tool
/// call and the extension-authoritative destination.
fn generated_send_message_id(call_id: &str, destination: &str) -> MessageFactId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"tau-ext-telegram/message.sent/v1\0");
    hasher.update(call_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(destination.as_bytes());
    MessageFactId::new(format!("telegram-send:{}", hasher.finalize().to_hex()))
}

/// Derive the opaque message reference required by
/// `SPEC-external-message-reports-and-facts`.
fn telegram_message_ref(conversation_id: &str, occurrence_id: &str) -> MessageFactId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"tau-ext-telegram/message-ref/v1\0");
    hasher.update(conversation_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(occurrence_id.as_bytes());
    MessageFactId::new(format!("telegram-message:{}", hasher.finalize().to_hex()))
}

/// Derive an opaque canonical sender reference without exposing a Telegram user
/// ID.
fn telegram_sender_ref(user_id: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"tau-ext-telegram/sender-ref/v1\0");
    hasher.update(user_id.as_bytes());
    format!("telegram-sender:{}", hasher.finalize().to_hex())
}

/// Bound a Telegram profile label to the universal message-fact display limit.
fn bounded_display_name(value: &str) -> Option<String> {
    let mut out = String::new();
    for ch in value.trim().chars().take(80) {
        if out.len() + ch.len_utf8() > 256 {
            break;
        }
        out.push(ch);
    }
    (!out.is_empty()).then_some(out)
}

fn is_private_message_chat(message: &TgMessage) -> bool {
    message
        .chat_type
        .as_deref()
        .is_none_or(|kind| kind == "private")
}

fn validate_api_base(api_base: &str) -> Result<(), String> {
    if api_base.is_empty() {
        return Err("telegram `api_base` must not be empty".to_owned());
    }
    let url = url::Url::parse(api_base)
        .map_err(|e| format!("telegram `api_base` must be a valid URL: {e}"))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err("telegram `api_base` must not include userinfo".to_owned());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("telegram `api_base` must not include query or fragment".to_owned());
    }
    match url.scheme() {
        "https" => Ok(()),
        "http" if url.host().is_some_and(is_loopback_host) => Ok(()),
        "http" => Err("telegram `api_base` may use http only for loopback hosts".to_owned()),
        _ => Err("telegram `api_base` must use https, or http for loopback tests".to_owned()),
    }
}

fn is_loopback_host(host: url::Host<&str>) -> bool {
    match host {
        url::Host::Domain(domain) => domain.eq_ignore_ascii_case("localhost"),
        url::Host::Ipv4(addr) => addr.is_loopback(),
        url::Host::Ipv6(addr) => addr.is_loopback(),
    }
}

impl Drop for Extension {
    fn drop(&mut self) {
        self.request_shutdown_detached();
    }
}

fn wait_for_poller_ready_or_shutdown(
    state_cell: &SharedState,
    shutdown: &AtomicBool,
) -> Option<PollRequest> {
    let mut state = state_cell.lock();
    state.retire_update_stream_lock_if_idle();
    while !state.shutdown_requested
        && (state.config.is_none()
            || state.registered_agents.is_empty()
            || state.update_stream_lock.is_none())
    {
        #[cfg(test)]
        state_cell.notify_readiness_wait_observer();
        state = state_cell.wait_while(state, |state| {
            state.retire_update_stream_lock_if_idle();
            !state.shutdown_requested
                && (state.config.is_none()
                    || state.registered_agents.is_empty()
                    || state.update_stream_lock.is_none())
        });
    }
    if state.shutdown_requested || shutdown.load(Ordering::Relaxed) {
        return None;
    }
    let cfg = state.config.clone()?;
    let update_stream_lock = state.update_stream_lock.clone()?;
    Some(PollRequest {
        cfg,
        offset: state.next_update_offset,
        config_generation: state.config_generation,
        coordination_generation: state.coordination_generation,
        update_stream_lock,
    })
}

fn wait_for_coordination_change_or_shutdown(
    state_cell: &SharedState,
    shutdown: &AtomicBool,
    delay: Duration,
    observed_generation: TelegramCoordinationGeneration,
) {
    if delay.is_zero() || shutdown.load(Ordering::Relaxed) {
        return;
    }
    let state = state_cell.lock();
    #[cfg(test)]
    state_cell.notify_wait_observer();
    let _guard = state_cell.wait_timeout_while(state, delay, |state| {
        !state.shutdown_requested && state.coordination_generation == observed_generation
    });
}

#[cfg(test)]
fn poll_loop(
    state: Arc<SharedState>,
    client: Arc<dyn TelegramClient>,
    output: Output,
    shutdown: Arc<AtomicBool>,
) {
    poll_loop_with_tool_names(
        state,
        client,
        output,
        shutdown,
        ToolNames::logical(),
        Arc::new(Mutex::new(())),
    );
}

fn poll_loop_with_tool_names(
    state: Arc<SharedState>,
    client: Arc<dyn TelegramClient>,
    output: Output,
    shutdown: Arc<AtomicBool>,
    tool_names: ToolNames,
    poller_publication: Arc<Mutex<()>>,
) {
    #[cfg(test)]
    struct Exit(Arc<dyn TelegramClient>);
    #[cfg(test)]
    impl Drop for Exit {
        fn drop(&mut self) {
            self.0.poller_exited();
        }
    }
    #[cfg(test)]
    let _exit = Exit(Arc::clone(&client));
    let ext = Extension {
        state,
        client,
        gateway: Arc::new(Mutex::new(None)),
        gateway_supervisor: GatewaySupervisor::new(),
        config_apply: Mutex::new(()),
        output,
        shutdown: Arc::clone(&shutdown),
        tool_names,
        poller_handle: Mutex::new(None),
        poller_publication,
    };
    let mut pending_retry_backoff = PendingRetryBackoff::new();
    let mut previous_poll_offset = None;
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        let Some(poll_request) = wait_for_poller_ready_or_shutdown(&ext.state, &shutdown) else {
            return;
        };
        if previous_poll_offset != Some(poll_request.offset) {
            pending_retry_backoff.reset();
            previous_poll_offset = Some(poll_request.offset);
        }
        if !poll_request
            .update_stream_lock
            .covers(poll_request.cfg.stream_identity())
        {
            tracing::warn!(
                target: LOG_TARGET,
                "telegram poller skipped request because stream lock did not match config"
            );
            continue;
        }
        let mut request_cfg = poll_request.cfg.clone();
        let draining_initial_backlog = {
            let state = ext.state.lock();
            !state.poller_drained_initial_backlog
        };
        if draining_initial_backlog {
            request_cfg.poll_timeout_seconds = 0;
        }
        match ext.client.get_updates(
            &request_cfg,
            poll_request.offset.map(TelegramUpdateOffset::as_i64),
        ) {
            Ok(updates) => {
                if shutdown.load(Ordering::Relaxed) {
                    return;
                }
                if !ext.poll_response_matches_config(poll_request.config_generation) {
                    continue;
                }
                let mut stale_generation = false;
                let (draining, replaying_pending_during_drain) = {
                    let mut state = ext.state.lock();
                    if state.config_generation != poll_request.config_generation
                        || state.config.is_none()
                    {
                        stale_generation = true;
                        (false, false)
                    } else if !state.poller_drained_initial_backlog {
                        let replaying_pending = !state.live_checkpoints.is_empty();
                        if !replaying_pending
                            && let Some(max_update_id) = updates.iter().map(|u| u.update_id).max()
                        {
                            state.next_update_offset = Some(max_update_id.next_offset());
                        }
                        if updates.is_empty() {
                            state.poller_drained_initial_backlog = true;
                        }
                        (true, replaying_pending)
                    } else {
                        (false, false)
                    }
                };
                if stale_generation {
                    continue;
                }
                let _publication = ext
                    .poller_publication
                    .lock()
                    .expect("poller publication gate");
                if shutdown.load(Ordering::Relaxed) {
                    return;
                }
                if draining {
                    if replaying_pending_during_drain {
                        if ext.process_draining_batch(updates, poll_request.config_generation)
                            == ProcessingControl::Stop
                        {
                            ext.request_shutdown();
                            return;
                        }
                        drop(_publication);
                        wait_for_coordination_change_or_shutdown(
                            &ext.state,
                            &shutdown,
                            pending_retry_backoff.take_delay(),
                            poll_request.coordination_generation,
                        );
                    }
                    continue;
                }
                if updates.is_empty() {
                    drop(_publication);
                    wait_for_coordination_change_or_shutdown(
                        &ext.state,
                        &shutdown,
                        Duration::from_millis(50),
                        poll_request.coordination_generation,
                    );
                    continue;
                }
                if ext.process_update_batch(updates, poll_request.config_generation)
                    == ProcessingControl::Stop
                {
                    ext.request_shutdown();
                    return;
                }
                drop(_publication);
                if !ext.state.lock().live_checkpoints.is_empty() {
                    wait_for_coordination_change_or_shutdown(
                        &ext.state,
                        &shutdown,
                        pending_retry_backoff.take_delay(),
                        poll_request.coordination_generation,
                    );
                } else {
                    pending_retry_backoff.reset();
                }
            }
            Err(message) => {
                if !ext.poll_response_matches_config(poll_request.config_generation) {
                    continue;
                }
                if let Some(diagnostic) = log_telegram_poll_failure(&message) {
                    ext.fail_active_polling_with_notice(&poll_request.cfg, &diagnostic);
                    continue;
                }
                wait_for_coordination_change_or_shutdown(
                    &ext.state,
                    &shutdown,
                    Duration::from_secs(5),
                    poll_request.coordination_generation,
                );
            }
        }
    }
}

/// Emit one categorical polling warning and retain only the safe contention
/// notice.
fn log_telegram_poll_failure(failure: &TelegramApiFailure) -> Option<String> {
    if let Some(diagnostic) = telegram_contention_diagnostic(&failure.to_string()) {
        tracing::warn!(target: LOG_TARGET, "telegram update stream contention detected");
        Some(diagnostic)
    } else {
        tracing::warn!(target: LOG_TARGET, "telegram polling failed");
        None
    }
}

/// Runs the extension with no test-only manual Disconnect observer.
fn run_with_client<R, W>(
    reader: R,
    writer: W,
    client: Arc<dyn TelegramClient>,
) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    #[cfg(not(test))]
    {
        run_with_client_inner(reader, writer, client)
    }
    #[cfg(test)]
    {
        run_with_client_inner(reader, writer, client, None)
    }
}

/// Runs the extension while observing its manual Disconnect path in a unit
/// test.
#[cfg(test)]
fn run_with_client_observing_disconnect<R, W>(
    reader: R,
    writer: W,
    client: Arc<dyn TelegramClient>,
    disconnect_observer: mpsc::Sender<()>,
) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    run_with_client_inner(reader, writer, client, Some(disconnect_observer))
}

/// Runs the extension with an optional test-only manual Disconnect observer.
fn run_with_client_inner<R, W>(
    reader: R,
    writer: W,
    client: Arc<dyn TelegramClient>,
    #[cfg(test)] disconnect_observer: Option<mpsc::Sender<()>>,
) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    let mut runtime = tau_client::TauExtensionRunner::new(TelegramExtension)
        .start_manual_loop_deferred_startup_with_state(reader, writer, move |handle| {
            let ext = Extension::new(client, handle);
            #[cfg(test)]
            {
                let mut state = ext.state.lock();
                state.current_session_id =
                    Some(tau_proto::SessionId::parse("s1").expect("test session id"));
                state.registration_replay_complete = true;
            }
            TelegramRuntime {
                ext,
                desired_registration_storage: DesiredRegistrationStorage::default(),
            }
        })?;
    #[cfg(not(test))]
    {
        let storage = DesiredRegistrationStorage::rpc(runtime.extension_data_client());
        runtime.state_mut().desired_registration_storage = storage;
    }
    runtime.state().ext.output.install_waker(runtime.waker());
    let Some(configure) = read_initial_config(&mut runtime)? else {
        let state = runtime.finish()?;
        state.ext.request_shutdown();
        return Ok(());
    };
    match configure_tool_names(&configure, &mut runtime) {
        Ok(tool_names) => {
            if let Err(error) = send_startup_declarations(&mut runtime, &tool_names) {
                runtime.state().ext.output.report_known_mandatory_failure();
                runtime.state().ext.shutdown_after_runtime_error();
                let _ = runtime.finish();
                return Err(Box::new(error));
            }
        }
        Err(error) => {
            runtime.state().ext.clear_config_after_error();
            runtime.handle().config_error(error.to_string())?;
        }
    }
    let exit = match drive_manual_runtime(
        &mut runtime,
        #[cfg(test)]
        disconnect_observer,
    ) {
        Ok(exit) => exit,
        Err(error) => {
            runtime.state().ext.shutdown_after_runtime_error();
            let _ = runtime.finish();
            return Err(error);
        }
    };
    let state = match exit {
        ManualRuntimeExit::Disconnect => {
            runtime.state().ext.request_shutdown_detached();
            runtime.finish_detached()
        }
        ManualRuntimeExit::InputClosed => {
            runtime.state().ext.shutdown_after_runtime_error();
            runtime.finish()?
        }
    };
    state.ext.request_shutdown_detached();
    Ok(())
}

struct TelegramExtension;

impl TauExtension for TelegramExtension {
    type State = TelegramRuntime;

    fn name(&self) -> &'static str {
        "tau-ext-telegram"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        // This manual loop owns polling and gateway side channels. Generic
        // tau-client scope helpers still own all structural tool naming.
        builder.message_bridge();
    }
}

struct TelegramRuntime {
    /// Shared Telegram bridge state and background-worker coordination.
    ext: Extension,
    /// Session-scoped desired-registration storage owned by the manual loop.
    desired_registration_storage: DesiredRegistrationStorage,
}

/// Read the mandatory initial configuration used for namespaced startup
/// declarations.
fn read_initial_config(
    runtime: &mut tau_client::ManualExtensionRuntime<TelegramRuntime>,
) -> Result<Option<tau_proto::Configure>, Box<dyn Error>> {
    loop {
        match runtime.recv()? {
            ManualRuntimeInput::Message(tau_proto::HarnessOutputMessage::Configure(configure)) => {
                runtime.dispatch_one(tau_proto::HarnessOutputMessage::Configure(
                    configure.clone(),
                ))?;
                return Ok(Some(configure));
            }
            ManualRuntimeInput::Message(tau_proto::HarnessOutputMessage::Disconnect(_)) => {
                return Ok(None);
            }
            ManualRuntimeInput::InputClosed => return Ok(None),
            ManualRuntimeInput::Timeout => {}
            ManualRuntimeInput::Message(_) => {}
        }
    }
}

/// Apply initial config and install the tool names computed from that config.
fn configure_tool_names(
    configure: &tau_proto::Configure,
    runtime: &mut tau_client::ManualExtensionRuntime<TelegramRuntime>,
) -> Result<ToolNames, Box<dyn Error>> {
    let parsed = parse_ext_config(&configure.config)?;
    let tool_names = ToolNames::from_scope(runtime.handle().tool_name_scope()?)?;
    let runtime_cfg = parsed.validate(&configure.secrets)?;
    runtime.state_mut().ext.tool_names = tool_names.clone();
    runtime
        .state()
        .ext
        .apply_config(runtime_cfg, configure.state_dir.clone())?;
    runtime
        .state()
        .ext
        .set_publisher_name(configure.instance_name.clone());
    Ok(tool_names)
}

/// Publish instance-specific tool names and subscriptions before Ready.
fn send_startup_declarations(
    runtime: &mut tau_client::ManualExtensionRuntime<TelegramRuntime>,
    tool_names: &ToolNames,
) -> ClientResult<()> {
    let historical = [
        tau_proto::EventSelector::Exact(tau_proto::EventName::SESSION_STARTED),
        tau_proto::EventSelector::Exact(tau_proto::EventName::AGENT_DISPLAY_NAME_SET),
        tau_proto::EventSelector::Exact(tau_proto::EventName::AGENT_STARTED),
        tau_proto::EventSelector::Exact(tau_proto::EventName::SESSION_AGENT_LOADED),
        tau_proto::EventSelector::Exact(tau_proto::EventName::SESSION_AGENT_UNLOADED),
    ];
    let live = [tau_proto::EventSelector::Exact(
        tau_proto::EventName::TOOL_STARTED,
    )]
    .into_iter()
    .chain(historical.iter().cloned())
    .chain([
        tau_proto::EventSelector::Exact(tau_proto::EventName::SESSION_REPLAY_COMPLETE),
        tau_proto::EventSelector::Exact(tau_proto::EventName::SESSION_SHUTDOWN),
        tau_proto::EventSelector::Exact(tau_proto::EventName::MESSAGE_DELIVERED),
    ]);
    runtime.startup_subscribe_split(historical.clone(), live)?;
    runtime.startup_local_tool(tau_proto::ToolRegistrationDeclared {
        tool: register_tool_spec_for(tool_names),
        tool_group: Some(telegram_tool_group()),
        prompt_fragment: None,
    })?;
    runtime.startup_local_tool(tau_proto::ToolRegistrationDeclared {
        tool: send_tool_spec_for(tool_names),
        tool_group: Some(telegram_tool_group()),
        prompt_fragment: None,
    })?;
    runtime.startup_ready(Some("telegram ready".to_owned()))
}

/// Reason the manual runtime stopped reading harness input.
enum ManualRuntimeExit {
    /// The harness requested an explicit disconnect.
    Disconnect,
    /// The harness input stream closed.
    InputClosed,
}

/// Drive post-startup messages using handlers equivalent to the former static
/// runner.
fn drive_manual_runtime(
    runtime: &mut tau_client::ManualExtensionRuntime<TelegramRuntime>,
    #[cfg(test)] disconnect_observer: Option<mpsc::Sender<()>>,
) -> Result<ManualRuntimeExit, Box<dyn Error>> {
    loop {
        runtime.state().ext.output.check_mandatory_output()?;
        let poll = match runtime.try_recv() {
            Ok(poll) => poll,
            Err(error) => {
                runtime
                    .state()
                    .ext
                    .output
                    .observe_pre_dispatch_error(&error);
                return Err(Box::new(error));
            }
        };
        match poll {
            ManualRuntimePoll::Message(tau_proto::HarnessOutputMessage::Configure(configure)) => {
                handle_configure_message(runtime.state_mut(), configure)?;
            }
            ManualRuntimePoll::Message(tau_proto::HarnessOutputMessage::Deliver(delivery)) => {
                if delivery.replay {
                    if let Err(message) =
                        handle_replayed_event_value(runtime.state(), *delivery.event)
                    {
                        runtime.state().ext.output.config_error(message)?;
                        return Err(Box::new(IoError::other(
                            "durable Telegram registration replay failed",
                        )));
                    }
                    continue;
                }
                match *delivery.event {
                    Event::ToolStarted(invoke)
                        if runtime.state().ext.handles_tool(invoke.tool_name.as_str()) =>
                    {
                        runtime.state().ext.dispatch_tool_checked(
                            invoke,
                            &runtime.state().desired_registration_storage,
                        )?;
                    }
                    Event::ToolStarted(_) => {}
                    event => {
                        if let Err(message) = handle_live_event_value(runtime.state(), event) {
                            runtime.state().ext.output.config_error(message)?;
                            return Err(Box::new(IoError::other(
                                "durable Telegram registration reconciliation failed",
                            )));
                        }
                    }
                }
            }
            ManualRuntimePoll::Message(tau_proto::HarnessOutputMessage::Disconnect(_)) => {
                runtime.state().ext.output.check_mandatory_output()?;
                #[cfg(test)]
                if let Some(observer) = disconnect_observer {
                    let _ = observer.send(());
                }
                break Ok(ManualRuntimeExit::Disconnect);
            }
            ManualRuntimePoll::InputClosed => {
                runtime.state().ext.output.check_mandatory_output()?;
                break Ok(ManualRuntimeExit::InputClosed);
            }
            ManualRuntimePoll::Empty => runtime.wait_for_wake(),
            ManualRuntimePoll::Message(_) => {}
        }
    }
}

/// Apply a runtime reconfiguration and report errors explicitly to the harness.
fn handle_configure_message(
    runtime: &mut TelegramRuntime,
    configure: tau_proto::Configure,
) -> ClientResult<()> {
    let publisher_name = configure.instance_name.clone();
    let result = parse_ext_config(&configure.config)
        .and_then(|cfg| cfg.validate(&configure.secrets))
        .and_then(|cfg| {
            let session_id = runtime
                .ext
                .state
                .lock()
                .current_session_id
                .clone()
                .ok_or_else(|| {
                    "cannot reconfigure Telegram before observing session.started".to_owned()
                })?;
            let empty = BTreeSet::new();
            runtime
                .desired_registration_storage
                .store(&session_id, &empty)
                .map_err(|error| error.to_string())?;
            {
                let mut state = runtime.ext.state.lock();
                state.desired_registrations = empty;
                state.clear_active_bridge_state();
                state.mark_coordination_changed();
            }
            runtime.ext.state.notify_all();
            runtime.ext.apply_config(cfg, configure.state_dir)
        });
    if let Err(message) = result {
        runtime.ext.clear_config_after_error();
        runtime.ext.output.config_error(message)?;
    } else {
        runtime.ext.set_publisher_name(publisher_name);
    }
    Ok(())
}

/// Accumulates only membership and labels needed for restart reconciliation.
fn handle_replayed_event_value(runtime: &TelegramRuntime, event: Event) -> Result<(), String> {
    if let Event::SessionStarted(started) = &event {
        let state = runtime.ext.state.lock();
        match state.current_session_id.as_ref() {
            Some(current) if current != &started.session_id => {
                return Err(format!(
                    "immutable session mismatch: expected `{current}`, received `{}`",
                    started.session_id
                ));
            }
            Some(_) if state.session_start_observed => {}
            None => {
                drop(state);
                runtime.ext.load_desired_registrations(
                    &runtime.desired_registration_storage,
                    &started.session_id,
                )?;
            }
            Some(_) => {
                drop(state);
                runtime.ext.load_desired_registrations(
                    &runtime.desired_registration_storage,
                    &started.session_id,
                )?;
            }
        }
        runtime.ext.state.lock().session_start_observed = true;
    }
    let mut state = runtime.ext.state.lock();
    match event {
        Event::SessionStarted(started) if state.current_session_id.is_none() => {
            state.current_session_id = Some(started.session_id);
        }
        Event::SessionStarted(_) => {}
        Event::SessionAgentLoaded(loaded) => {
            state.replayed_loaded_agents.insert(loaded.agent_id);
        }
        Event::SessionAgentUnloaded(unloaded) => {
            state.replayed_loaded_agents.remove(&unloaded.agent_id);
        }
        Event::AgentStarted(started) => {
            if let Some(display_name) = started.display_name {
                state.agent_labels.insert(started.agent_id, display_name);
            }
        }
        Event::AgentDisplayNameSet(name) => {
            state.agent_labels.insert(name.agent_id, name.display_name);
        }
        _ => return Ok(()),
    }
    state.mark_coordination_changed();
    drop(state);
    runtime.ext.state.notify_all();
    Ok(())
}

/// Handle a delivered live event without tau-client's static handler registry.
fn handle_live_event_value(runtime: &TelegramRuntime, event: Event) -> Result<(), String> {
    match event {
        Event::MessageDelivered(fact) => {
            runtime.ext.acknowledge_live_delivery(&fact);
        }
        Event::AgentDisplayNameSet(name) => {
            let mut state = runtime.ext.state.lock();
            state
                .agent_labels
                .insert(name.agent_id.clone(), name.display_name.clone());
            state.mark_coordination_changed();
            runtime.ext.state.notify_all();
            if let (Some(gateway), Some(session_id)) = (
                runtime.ext.gateway_client(),
                state.current_session_id.clone(),
            ) && state.registered_agents.contains(&name.agent_id)
            {
                drop(state);
                match gateway.register_agent(
                    session_id.as_ref(),
                    name.agent_id.as_ref(),
                    Some(name.display_name),
                ) {
                    Ok(response) => {
                        let _ = runtime.ext.apply_gateway_response(&gateway, response);
                    }
                    Err(error) if error.is_connection_fatal() => {
                        fail_gateway_client_if_current(
                            &runtime.ext.gateway,
                            &runtime.ext.state,
                            &gateway,
                        );
                    }
                    Err(_) => {}
                }
            }
        }
        Event::SessionStarted(started) => {
            let state = runtime.ext.state.lock();
            match state.current_session_id.as_ref() {
                Some(current) if current != &started.session_id => {
                    return Err(format!(
                        "immutable session mismatch: expected `{current}`, received `{}`",
                        started.session_id
                    ));
                }
                Some(_) if state.session_start_observed => return Ok(()),
                Some(_) => {}
                None => {}
            }
            drop(state);
            runtime.ext.load_desired_registrations(
                &runtime.desired_registration_storage,
                &started.session_id,
            )?;
            let mut state = runtime.ext.state.lock();
            state.current_session_id = Some(started.session_id);
            state.session_start_observed = true;
            state.registration_replay_complete = false;
            state.mark_coordination_changed();
            runtime.ext.state.notify_all();
        }
        Event::AgentStarted(started) => {
            if let Some(display_name) = started.display_name.clone() {
                let mut state = runtime.ext.state.lock();
                state.agent_labels.insert(started.agent_id, display_name);
            }
        }
        Event::SessionAgentUnloaded(unloaded) => {
            let persistence_error = runtime
                .ext
                .persist_desired_registration(
                    &runtime.desired_registration_storage,
                    &unloaded.session_id,
                    &unloaded.agent_id,
                    false,
                )
                .err();
            let mut state = runtime.ext.state.lock();
            let session_id = state
                .current_session_id
                .clone()
                .or(Some(unloaded.session_id.clone()));
            state.registered_agents.remove(&unloaded.agent_id);
            state.agent_labels.remove(&unloaded.agent_id);
            state
                .selected_agent_by_chat
                .retain(|_, agent_id| agent_id != &unloaded.agent_id);
            if state.registered_agents.is_empty() {
                state.poller_drained_initial_backlog = false;
            }
            state.mark_coordination_changed();
            runtime.ext.state.notify_all();
            drop(state);
            if let (Some(gateway), Some(session_id)) = (runtime.ext.gateway_client(), session_id) {
                match gateway.unregister_agent(session_id.as_ref(), unloaded.agent_id.as_ref()) {
                    Ok(response) => {
                        let _ = runtime.ext.apply_gateway_response(&gateway, response);
                    }
                    Err(error) => {
                        if error.is_connection_fatal() {
                            fail_gateway_client_if_current(
                                &runtime.ext.gateway,
                                &runtime.ext.state,
                                &gateway,
                            );
                        }
                    }
                }
            }
            if let Some(message) = persistence_error {
                return Err(message.to_string());
            }
        }
        Event::SessionReplayComplete(complete) => {
            if complete.error.is_some() {
                return Err(
                    "session replay failed before Telegram registration reconciliation".to_owned(),
                );
            }
            if runtime.ext.state.lock().current_session_id.as_ref() != Some(&complete.session_id) {
                return Err("stale Telegram session replay completion".to_owned());
            }
            runtime.ext.reconcile_desired_registrations(
                &runtime.desired_registration_storage,
                &complete.session_id,
            )?;
        }
        Event::SessionShutdown(shutdown) => {
            let (session_id, agents) = {
                let mut state = runtime.ext.state.lock();
                if state.current_session_id.as_ref() != Some(&shutdown.session_id) {
                    return Err(
                        "Telegram session shutdown does not match immutable binding".to_owned()
                    );
                }
                let session_id = state.current_session_id.clone();
                let agents = state.registered_agents.iter().cloned().collect::<Vec<_>>();
                state.registered_agents.clear();
                state.desired_registrations.clear();
                state.replayed_loaded_agents.clear();
                state.registration_replay_complete = false;
                state.agent_labels.clear();
                state.selected_agent_by_chat.clear();
                state.poller_drained_initial_backlog = false;
                state.update_stream_lock = None;
                state.mark_coordination_changed();
                runtime.ext.state.notify_all();
                (session_id, agents)
            };
            let gateway = runtime.ext.gateway_client();
            if let (Some(gateway), Some(session_id)) = (gateway.as_ref(), session_id.as_ref()) {
                for agent_id in &agents {
                    match gateway.unregister_agent(session_id.as_ref(), agent_id.as_ref()) {
                        Ok(response) => {
                            if !runtime.ext.apply_gateway_response(gateway, response) {
                                break;
                            }
                        }
                        Err(error) if error.is_connection_fatal() => {
                            fail_gateway_client_if_current(
                                &runtime.ext.gateway,
                                &runtime.ext.state,
                                gateway,
                            );
                            break;
                        }
                        Err(_) => {}
                    }
                }
            }
            if gateway.is_none() {
                let mut state = runtime.ext.state.lock();
                state.shutdown_requested = true;
                state.mark_coordination_changed();
                runtime.ext.shutdown.store(true, Ordering::Relaxed);
                runtime.ext.state.notify_all();
            }
        }
        _ => {}
    }
    Ok(())
}

fn parse_ext_config(value: &CborValue) -> Result<ExtConfig, String> {
    value.deserialized().map_err(|e| e.to_string())
}

fn telegram_tool_group() -> tau_proto::ToolGroup {
    telegram_tool_group_for(&ToolNames::logical())
}

fn telegram_tool_group_for(tool_names: &ToolNames) -> tau_proto::ToolGroup {
    tau_proto::ToolGroup {
        name: tool_names.group.clone(),
        prompt_fragment: None,
    }
}

fn example_field(name: &str, value: CborValue) -> (CborValue, CborValue) {
    (CborValue::Text(name.to_owned()), value)
}

fn example_text(value: &str) -> CborValue {
    CborValue::Text(value.to_owned())
}

#[cfg(test)]
fn register_tool_spec() -> ToolSpec {
    register_tool_spec_for(&ToolNames::logical())
}

fn register_tool_spec_for(tool_names: &ToolNames) -> ToolSpec {
    ToolSpec {
        name: tau_proto::ToolName::new(REGISTER_TOOL_NAME),
        model_visible_name: Some(tau_proto::ToolName::new(REGISTER_TOOL_NAME)),
        description: Some(format!(
            "Register or unregister this agent for Telegram messages through the `{}` bot namespace. Use enabled=true to allow an allowlisted Telegram user to send prompts to this agent; use enabled=false to stop listening. When replying to Telegram-originated prompts, use {}.",
            tool_names.namespace, tool_names.send
        )),
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": { "enabled": { "type": "boolean" } },
            "required": ["enabled"],
            "additionalProperties": false
        })),
        format: None,
        tags: vec![tau_proto::ToolTag::new(REGISTER_TOOL_TAG)],
        enabled_by_default: false,
        background_support: None,
        examples: vec![ToolExample {
            id: "enable-registration".to_owned(),
            title: Some("Register for Telegram".to_owned()),
            arguments: CborValue::Map(vec![example_field("enabled", CborValue::Bool(true))]),
            note: Some("Use enabled=false to stop receiving Telegram prompts.".to_owned()),
            subcommand: None,
        }],
    }
}

#[cfg(test)]
fn send_tool_spec() -> ToolSpec {
    send_tool_spec_for(&ToolNames::logical())
}

fn send_tool_spec_for(tool_names: &ToolNames) -> ToolSpec {
    ToolSpec {
        name: tau_proto::ToolName::new(SEND_TOOL_NAME),
        model_visible_name: Some(tau_proto::ToolName::new(SEND_TOOL_NAME)),
        description: Some(format!(
            "Send a text message to the configured or linked Telegram chat for the `{}` bot namespace. Only registered agents may use this tool; it cannot choose arbitrary chat ids. Use it to answer Telegram-originated message facts.",
            tool_names.namespace
        )),
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": { "message": { "type": "string" } },
            "required": ["message"],
            "additionalProperties": false
        })),
        format: None,
        tags: vec![tau_proto::ToolTag::new(SEND_TOOL_TAG)],
        enabled_by_default: false,
        background_support: None,
        examples: vec![ToolExample {
            id: "send-reply".to_owned(),
            title: Some("Send a Telegram reply".to_owned()),
            arguments: CborValue::Map(vec![example_field(
                "message",
                example_text("Thanks, I’ll look into it."),
            )]),
            note: Some(
                "There is no chat_id argument; the configured or linked chat is used.".to_owned(),
            ),
            subcommand: None,
        }],
    }
}

fn cbor_bool_field(arguments: &CborValue, field: &str) -> Result<bool, String> {
    let CborValue::Map(entries) = arguments else {
        return Err("arguments must be an object".to_owned());
    };
    for (key, value) in entries {
        if let CborValue::Text(name) = key
            && name == field
        {
            return match value {
                CborValue::Bool(value) => Ok(*value),
                _ => Err(format!("`{field}` must be a boolean")),
            };
        }
    }
    Err(format!("missing required argument `{field}`"))
}

fn validate_object_fields(arguments: &CborValue, allowed_fields: &[&str]) -> Result<(), String> {
    let CborValue::Map(entries) = arguments else {
        return Err("arguments must be an object".to_owned());
    };
    for (key, _) in entries {
        let CborValue::Text(name) = key else {
            return Err("argument field names must be strings".to_owned());
        };
        if !allowed_fields.contains(&name.as_str()) {
            return Err(format!("unknown argument `{name}`"));
        }
    }
    Ok(())
}

fn cbor_string_field(arguments: &CborValue, field: &str) -> Result<String, String> {
    let CborValue::Map(entries) = arguments else {
        return Err("arguments must be an object".to_owned());
    };
    for (key, value) in entries {
        if let CborValue::Text(name) = key
            && name == field
        {
            return match value {
                CborValue::Text(value) => Ok(value.clone()),
                _ => Err(format!("`{field}` must be a string")),
            };
        }
    }
    Err(format!("missing required argument `{field}`"))
}

fn tool_result(invoke: ToolStarted, text: &str) -> Event {
    Event::ToolResult(ToolResult {
        presentation: Default::default(),
        call_id: invoke.call_id,
        tool_name: invoke.tool_name,
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text(text.to_owned()),
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

fn agent_display_name<'a>(state: &'a State, agent_id: &AgentId) -> Option<&'a str> {
    state
        .agent_labels
        .get(agent_id)
        .map(String::as_str)
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .filter(|label| *label != agent_id.as_ref())
}

fn agent_designator(state: &State, agent_id: &AgentId) -> String {
    let id = agent_id.as_ref();
    match agent_display_name(state, agent_id) {
        Some(display_name) => format!("{id} ({display_name})"),
        None => id.to_owned(),
    }
}

fn agents_text(state: &State) -> String {
    if state.registered_agents.is_empty() {
        return "No Tau agents are registered.".to_owned();
    }
    let mut lines = vec!["Registered Tau agents:".to_owned()];
    let mut agents = state.registered_agents.iter().collect::<Vec<_>>();
    agents.sort();
    for agent_id in agents {
        lines.push(format!("- {}", agent_designator(state, agent_id)));
    }
    lines.join("\n")
}

fn resolve_agent(state: &State, query: &str) -> Result<AgentId, String> {
    let query = query.trim();
    let mut matches = state
        .registered_agents
        .iter()
        .filter(|agent_id| agent_id.as_ref() == query || agent_id.as_ref().starts_with(query));
    let Some(first) = matches.next() else {
        return Err(format!("No registered Tau agent matches `{query}`."));
    };
    if matches.next().is_some() {
        return Err(format!("Multiple registered Tau agents match `{query}`."));
    }
    Ok(first.clone())
}

fn split_first(s: &str) -> (&str, &str) {
    match s.trim().split_once(char::is_whitespace) {
        Some((first, rest)) => (first, rest),
        None => (s.trim(), ""),
    }
}

fn parse_command(text: &str) -> (Option<&str>, &str) {
    if !text.starts_with('/') {
        return (None, "");
    }
    let (token, rest) = split_first(text);
    let command = token.split_once('@').map_or(token, |(command, _)| command);
    (Some(command), rest)
}

fn help_text() -> &'static str {
    "Tau Telegram bridge linked. Commands: /agents, /select <agent-id-or-prefix>, /to <agent-id-or-prefix> <message>. Plain text goes to the selected agent, or to the only registered agent."
}

struct HttpTelegramClient {
    agent: ureq::Agent,
}

impl HttpTelegramClient {
    fn agent() -> ureq::Agent {
        let tls_config = path_ureq_tls::TlsConfig::builder()
            .root_certs(path_ureq_tls::RootCerts::PlatformVerifier)
            .build();
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(HTTP_TIMEOUT))
            .http_status_as_error(false)
            .tls_config(tls_config)
            .build();
        ureq::Agent::new_with_config(config)
    }
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            bot_token: String::new(),
            allowed_user_ids: HashSet::new(),
            configured_chat_id: None,
            api_base: DEFAULT_API_BASE.to_owned(),
            poll_timeout_seconds: DEFAULT_POLL_TIMEOUT_SECONDS,
        }
    }
}

impl Default for HttpTelegramClient {
    fn default() -> Self {
        Self {
            agent: Self::agent(),
        }
    }
}

impl TelegramClient for HttpTelegramClient {
    fn get_webhook_info(&self, cfg: &RuntimeConfig) -> Result<TgWebhookInfo, TelegramApiFailure> {
        let value = self.post(cfg, "getWebhookInfo", serde_json::json!({}))?;
        decode_webhook_info(cfg, &value).map_err(TelegramApiFailure::Protocol)
    }

    fn get_updates(
        &self,
        cfg: &RuntimeConfig,
        offset: Option<i64>,
    ) -> Result<Vec<TgUpdate>, TelegramApiFailure> {
        let mut body = serde_json::json!({
            "timeout": cfg.poll_timeout_seconds,
            "allowed_updates": ["message"],
        });
        if let Some(offset) = offset {
            body["offset"] = serde_json::json!(offset);
        }
        let value = self.post(cfg, "getUpdates", body)?;
        let result = value
            .get("result")
            .and_then(|value| value.as_array())
            .ok_or_else(|| {
                TelegramApiFailure::Protocol(
                    "Telegram getUpdates response missing result array".to_owned(),
                )
            })?;
        decode_updates(result).map_err(TelegramApiFailure::Protocol)
    }

    fn send_message(
        &self,
        cfg: &RuntimeConfig,
        chat_id: i64,
        text: &str,
    ) -> Result<(), TelegramApiFailure> {
        self.post(
            cfg,
            "sendMessage",
            serde_json::json!({ "chat_id": chat_id, "text": text }),
        )?;
        Ok(())
    }
}

impl HttpTelegramClient {
    fn post(
        &self,
        cfg: &RuntimeConfig,
        method: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, TelegramApiFailure> {
        let url = format!("{}/bot{}/{}", cfg.api_base, cfg.bot_token, method);
        let mut response = self
            .agent
            .post(&url)
            .content_type("application/json")
            .send(body.to_string())
            .map_err(|_error| TelegramApiFailure::Transport)?;
        let status = response.status();
        if !status.is_success() {
            let bytes = response
                .body_mut()
                .with_config()
                .limit(1025)
                .read_to_vec()
                .unwrap_or_default();
            let text = String::from_utf8_lossy(&bytes);
            let text = cfg.stream_identity().redact_token(&text);
            return Err(TelegramApiFailure::Http {
                status: status.as_u16(),
                message: bounded_api_diagnostic(&text),
            });
        }
        let text = response
            .body_mut()
            .with_config()
            .limit(MAX_SUCCESSFUL_RESPONSE_BODY_BYTES + 1)
            .lossy_utf8(true)
            .read_to_string()
            .map_err(|error| {
                TelegramApiFailure::Protocol(format!(
                    "reading Telegram HTTP {status} response: {error}"
                ))
            })?;
        serde_json::from_str(&text).map_err(|error| {
            TelegramApiFailure::Protocol(format!("invalid Telegram JSON: {error}"))
        })
    }
}

/// Bound a remote diagnostic at a UTF-8 boundary before displaying it.
fn bounded_api_diagnostic(text: &str) -> String {
    const MAX_BYTES: usize = 1024;
    let mut output = String::new();
    for character in text.chars() {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        if MAX_BYTES < output.len() + character.len_utf8() {
            break;
        }
        output.push(character);
    }
    output
}

fn decode_webhook_info(
    cfg: &RuntimeConfig,
    value: &serde_json::Value,
) -> Result<TgWebhookInfo, String> {
    let result = value
        .get("result")
        .ok_or_else(|| "Telegram getWebhookInfo response missing result".to_owned())?;
    let url = result
        .get("url")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_owned();
    let pending_update_count = result
        .get("pending_update_count")
        .and_then(|value| value.as_i64());
    let last_error_message = result
        .get("last_error_message")
        .and_then(|value| value.as_str())
        .map(|message| cfg.stream_identity().redact_token(message));
    Ok(TgWebhookInfo {
        url,
        pending_update_count,
        last_error_message,
    })
}

fn decode_update(value: &serde_json::Value) -> Result<Option<TgUpdate>, String> {
    let Some(raw_update_id) = value.get("update_id").and_then(serde_json::Value::as_i64) else {
        return Ok(None);
    };
    let update_id = TelegramUpdateId::new(raw_update_id).ok_or_else(|| {
        "Telegram getUpdates returned an update id outside the supported offset range".to_owned()
    })?;
    let message = decode_message(value);
    Ok(Some(TgUpdate { update_id, message }))
}

/// Decode one Bot API update array and reject IDs whose exclusive successor
/// cannot be represented on the next request.
fn decode_updates(values: &[serde_json::Value]) -> Result<Vec<TgUpdate>, String> {
    let mut updates = Vec::new();
    for value in values {
        if let Some(update) = decode_update(value)? {
            updates.push(update);
        }
    }
    Ok(updates)
}

fn decode_message(value: &serde_json::Value) -> Option<TgMessage> {
    let msg = value.get("message")?;
    let chat = msg.get("chat")?;
    let chat_id = chat.get("id")?.as_i64()?;
    let chat_type = chat
        .get("type")
        .and_then(|value| value.as_str())
        .map(str::to_owned);
    let from = msg.get("from")?;
    let user_id = from.get("id")?.as_i64()?;
    let from_name = from
        .get("username")
        .or_else(|| from.get("first_name"))
        .and_then(|value| value.as_str())
        .map(str::to_owned);
    let text = msg
        .get("text")
        .and_then(|value| value.as_str())
        .map(str::to_owned);
    Some(TgMessage {
        chat_id,
        chat_type,
        user_id,
        from_name,
        text,
    })
}

#[cfg(test)]
mod tests;
