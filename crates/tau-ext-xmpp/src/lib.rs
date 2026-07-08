//! Personal XMPP bridge extension for Tau agents.
//!
//! The extension exposes `xmpp_register` and `xmpp_send`. It is disabled by
//! default, uses a mandatory JID allowlist, and treats XMPP text as external
//! untrusted prompt input.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use futures_util::StreamExt;
use rand::RngCore;
use tau_client::{ClientError, ClientHandle, ClientResult, ExtensionBuilder, TauExtension};
use tau_proto::{
    AgentId, CborValue, Event, ExtPromptSubmitRequest, HarnessInputMessage, SessionId, ToolError,
    ToolExample, ToolProgress, ToolResult, ToolSpec, ToolStarted, ToolUseState, ToolUseStatus,
};
use tokio_xmpp::{Client, IqRequest, IqResponse};
use xmpp_parsers::delay::Delay;
use xmpp_parsers::iq::Iq;
use xmpp_parsers::jid::{BareJid, Jid};
use xmpp_parsers::message::{Lang, Message, MessageType};
use xmpp_parsers::muc::muc::History;
use xmpp_parsers::muc::user::{Invite, Status as MucStatus};
use xmpp_parsers::muc::{Muc, MucUser};
use xmpp_parsers::ns;
use xmpp_parsers::presence::{Presence, Type as PresenceType};
use xmpp_parsers::stanza::Stanza;
use xmpp_parsers::stanza_error::StanzaError;

/// Tracing target used by this extension.
pub const LOG_TARGET: &str = "xmpp";

/// Internal tool name for registering the current agent as an XMPP listener.
pub const REGISTER_TOOL_NAME: &str = "xmpp_register";

/// Internal tool name for sending an XMPP message from a registered agent.
pub const SEND_TOOL_NAME: &str = "xmpp_send";

/// Tool group name shared by all XMPP bridge tools.
pub const TOOL_GROUP_NAME: &str = "xmpp";

/// Tag marking tools that register an agent with the XMPP bridge.
pub const REGISTER_TOOL_TAG: &str = "xmpp:register";

/// Tag marking tools that send messages through the XMPP bridge.
pub const SEND_TOOL_TAG: &str = "xmpp:send";

const DEFAULT_RESOURCE_PREFIX: &str = "tau";
const DEFAULT_ROOM_PREFIX: &str = "tau";
const DEFAULT_MESSAGE_LIMIT: usize = 16 * 1024;
const MAX_MESSAGE_LIMIT: usize = 128 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const REGISTER_TIMEOUT: Duration = Duration::from_secs(45);
const ONLINE_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const READY_RESPONSE_SLACK: Duration = Duration::from_secs(1);
const STANZA_TIMEOUT: Duration = Duration::from_secs(20);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const WORKER_SHUTDOWN_CLEANUP_TIMEOUT: Duration = Duration::from_secs(4);
const MUC_OWNER_NS: &str = "http://jabber.org/protocol/muc#owner";
const MUC_ROOM_DISAMBIGUATOR_BYTES: usize = 5;
const MUC_SESSION_SLUG_MAX_CHARS: usize = 16;
const MUC_AGENT_SLUG_MAX_CHARS: usize = 18;

/// Run the XMPP extension over stdio.
pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    tau_client::init_logging_for(LOG_TARGET);
    run(std::io::stdin(), std::io::stdout())
}

/// Run the XMPP extension over an arbitrary transport.
pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write + Send + 'static,
{
    run_with_bridge(reader, writer, Arc::new(LiveXmppBridge::default()))
}

#[derive(Clone)]
enum Output {
    /// Test-side output channel preserving existing direct unit-test helpers.
    Channel(mpsc::Sender<HarnessInputMessage>),
    /// Tau-client output handle used by the protocol runtime.
    Client(ClientHandle),
}

impl From<mpsc::Sender<HarnessInputMessage>> for Output {
    fn from(tx: mpsc::Sender<HarnessInputMessage>) -> Self {
        Self::Channel(tx)
    }
}

impl From<ClientHandle> for Output {
    fn from(handle: ClientHandle) -> Self {
        Self::Client(handle)
    }
}

impl Output {
    /// Sends one protocol frame, intentionally ignoring closed-writer failures.
    ///
    /// XMPP worker and tool output is best-effort once the harness has
    /// disconnected or the tau-client writer has shut down.
    fn send(&self, message: HarnessInputMessage) {
        match self {
            Self::Channel(tx) => {
                let _ = tx.send(message);
            }
            Self::Client(handle) => {
                let _ = handle.send_detached(message);
            }
        }
    }

    /// Emits one event through the harness output channel.
    fn emit(&self, event: Event) {
        self.send(HarnessInputMessage::emit(event));
    }
}

/// Shared shutdown state that supports both synchronous checks and async
/// wakeups.
struct ShutdownSignal {
    /// Fast flag used by synchronous call sites that cannot await.
    requested: AtomicBool,
    /// Wakes async worker tasks as soon as shutdown is requested.
    notify: tokio::sync::Notify,
}

impl ShutdownSignal {
    /// Create a shutdown signal in the running state.
    fn new() -> Self {
        Self {
            requested: AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
        }
    }

    /// Return whether shutdown has already been requested.
    fn is_requested(&self) -> bool {
        self.requested.load(Ordering::Relaxed)
    }

    /// Request shutdown and wake async waiters immediately.
    fn request(&self) {
        self.requested.store(true, Ordering::Relaxed);
        self.notify.notify_waiters();
    }

    /// Wait until shutdown is requested without polling.
    async fn wait(&self) {
        loop {
            // Create the notification future before checking the flag: `notify_waiters()`
            // does not buffer for future waiters, so this ordering prevents missing a
            // concurrent request between the check and await.
            let notified = self.notify.notified();
            if self.is_requested() {
                return;
            }
            notified.await;
        }
    }
}

/// Small bridge surface used by the extension and faked by unit tests.
trait XmppBridge: Send + Sync + 'static {
    /// Ensure the underlying XMPP task is started.
    fn ensure_started(
        &self,
        cfg: RuntimeConfig,
        output: Output,
        shutdown: Arc<ShutdownSignal>,
    ) -> Result<(), String>;

    /// Register one agent conversation and return its XMPP address.
    fn register_agent(
        &self,
        cfg: &RuntimeConfig,
        session_id: &SessionId,
        agent_id: &AgentId,
    ) -> Result<String, String>;

    /// Remove one registered agent conversation from the bridge.
    fn unregister_agent(&self, agent_id: &AgentId) -> Result<(), String>;

    /// Wait for the underlying XMPP stream to be online and authenticated.
    fn wait_until_ready(&self, timeout: Duration) -> Result<(), String>;

    /// Send text to the registered agent's conversation.
    fn send_message(&self, agent_id: &AgentId, text: &str) -> Result<(), String>;

    /// Request bridge shutdown and wait briefly for best-effort cleanup.
    fn shutdown(&self, timeout: Duration) -> Result<(), String>;
}

/// Validated runtime configuration, including resolved secret values.
#[derive(Clone)]
struct RuntimeConfig {
    /// Bare XMPP account JID used for login.
    account_jid: BareJid,
    /// Resolved account password. Never log this value.
    password: String,
    /// JIDs allowed to submit prompts through this bridge.
    allowed_jids: Vec<AllowedJid>,
    /// Default human recipient for notices and direct fallback sends.
    default_recipient: Jid,
    /// Routing mode used for registered conversations.
    routing_mode: RoutingMode,
    /// Prefix for generated resource strings.
    resource_prefix: String,
    /// MUC options used in MUC routing mode.
    muc: MucConfig,
    /// Maximum accepted outbound or inbound text length.
    max_message_bytes: usize,
    /// Optional extension instance name for generated resources/rooms.
    instance_name: Option<String>,
}

/// Raw deserialized extension config from `harness.yaml`.
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ExtConfig {
    /// Bare XMPP account JID used for login.
    jid: Option<String>,
    /// Secret name carrying the XMPP password.
    password_secret: Option<String>,
    /// JIDs allowed to submit prompts.
    allowed_jids: Vec<String>,
    /// Default human recipient for notices and direct fallback sends.
    default_recipient: Option<String>,
    /// Routing mode configuration.
    routing: RoutingConfig,
    /// Prefix for generated resource strings.
    resource_prefix: Option<String>,
    /// MUC-specific configuration.
    muc: MucConfigRaw,
    /// Optional maximum text size in bytes.
    max_message_bytes: Option<usize>,
}

/// Raw routing config.
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RoutingConfig {
    /// Routing mode name: `muc` or `direct_resource`.
    mode: Option<String>,
}

/// Raw MUC config.
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct MucConfigRaw {
    /// MUC service domain, for example `conference.example.org`.
    service: Option<String>,
    /// Room localpart prefix.
    room_prefix: Option<String>,
    /// Whether Tau requires real JID exposure in room presence.
    expose_real_jids: Option<bool>,
    /// Explicitly trust server-side room membership when real JIDs are hidden.
    trust_muc_membership: Option<bool>,
    /// Send an initial notice to the default recipient with the room JID.
    invite_default_recipient: Option<bool>,
}

/// Validated routing mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RoutingMode {
    /// Route each registered agent through a MUC room.
    Muc,
    /// Route through the extension's exact bound full-resource JID.
    DirectResource,
}

/// Validated MUC config.
#[derive(Clone)]
struct MucConfig {
    /// MUC service JID.
    service: Option<BareJid>,
    /// Room localpart prefix.
    room_prefix: String,
    /// Whether the deployment is expected to expose real JIDs in presence.
    expose_real_jids: bool,
    /// Whether membership may be trusted when real JIDs are hidden.
    trust_muc_membership: bool,
    /// Whether to send the room notice to the default recipient.
    invite_default_recipient: bool,
}

/// One allowed sender JID entry.
#[derive(Clone, Debug, Eq, PartialEq)]
enum AllowedJid {
    /// Bare JID entry; matches any resource for the account.
    Bare(BareJid),
    /// Full JID entry; matches exactly.
    Full(Jid),
}

impl AllowedJid {
    /// Parse one allowlist entry.
    fn parse(text: &str) -> Result<Self, String> {
        let jid =
            Jid::new(text).map_err(|e| format!("invalid allowed_jids entry `{text}`: {e}"))?;
        if jid.resource().is_some() {
            Ok(Self::Full(jid))
        } else {
            Ok(Self::Bare(jid.to_bare()))
        }
    }

    /// Return whether this allowlist entry accepts a sender JID.
    fn matches(&self, jid: &Jid) -> bool {
        match self {
            Self::Bare(bare) => &jid.to_bare() == bare,
            Self::Full(full) => jid == full,
        }
    }
}

impl RuntimeConfig {
    /// Return whether a sender JID is allowlisted.
    fn is_allowed(&self, jid: &Jid) -> bool {
        self.allowed_jids.iter().any(|allowed| allowed.matches(jid))
    }
}

impl ExtConfig {
    /// Validate raw config and resolve the password secret.
    fn validate(
        self,
        secrets: &BTreeMap<String, tau_proto::SecretValue>,
        instance_name: Option<String>,
    ) -> Result<RuntimeConfig, String> {
        let account_jid = validate_account_jid(self.jid)?;
        let password = resolve_password(secrets, self.password_secret)?;
        let allowed_jids = validate_allowed_jids(self.allowed_jids)?;
        let default_recipient = validate_default_recipient(self.default_recipient, &allowed_jids)?;
        let routing_mode = self.routing.validate()?;
        let muc = self.muc.validate(routing_mode)?;
        let max_message_bytes = validate_max_message_bytes(self.max_message_bytes)?;
        Ok(RuntimeConfig {
            account_jid,
            password,
            allowed_jids,
            default_recipient,
            routing_mode,
            resource_prefix: validate_resource_prefix(self.resource_prefix),
            muc,
            max_message_bytes,
            instance_name,
        })
    }
}

impl RoutingConfig {
    /// Validate the configured routing mode.
    fn validate(self) -> Result<RoutingMode, String> {
        match self.mode.as_deref().unwrap_or("muc") {
            "muc" => Ok(RoutingMode::Muc),
            "direct_resource" => Ok(RoutingMode::DirectResource),
            other => Err(format!("unsupported xmpp routing.mode `{other}`")),
        }
    }
}

impl MucConfigRaw {
    /// Validate MUC-specific options for the selected routing mode.
    fn validate(self, routing_mode: RoutingMode) -> Result<MucConfig, String> {
        let service = validate_muc_service(self.service)?;
        if routing_mode == RoutingMode::Muc && service.is_none() {
            return Err("xmpp routing.mode `muc` requires `muc.service`".to_owned());
        }
        Ok(MucConfig {
            service,
            room_prefix: validate_room_prefix(self.room_prefix),
            expose_real_jids: self.expose_real_jids.unwrap_or(true),
            trust_muc_membership: self.trust_muc_membership.unwrap_or(false),
            invite_default_recipient: self.invite_default_recipient.unwrap_or(true),
        })
    }
}

fn validate_account_jid(jid: Option<String>) -> Result<BareJid, String> {
    let account_text = jid.ok_or_else(|| "xmpp config requires `jid`".to_owned())?;
    let account = Jid::new(&account_text).map_err(|e| format!("invalid xmpp `jid`: {e}"))?;
    if account.resource().is_some() {
        return Err(
            "xmpp `jid` must be a bare account JID; Tau generates unique resources".to_owned(),
        );
    }
    Ok(account.to_bare())
}

fn resolve_password(
    secrets: &BTreeMap<String, tau_proto::SecretValue>,
    password_secret: Option<String>,
) -> Result<String, String> {
    let secret_name =
        password_secret.ok_or_else(|| "xmpp config requires `password_secret`".to_owned())?;
    secrets
        .get(&secret_name)
        .map(tau_proto::SecretValue::expose_secret)
        .filter(|password| !password.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("xmpp secret `{secret_name}` is missing or empty"))
}

fn validate_allowed_jids(entries: Vec<String>) -> Result<Vec<AllowedJid>, String> {
    if entries.is_empty() {
        return Err("xmpp config requires non-empty `allowed_jids`".to_owned());
    }
    entries
        .iter()
        .map(|entry| AllowedJid::parse(entry))
        .collect::<Result<Vec<_>, _>>()
}

fn validate_default_recipient(
    default_recipient: Option<String>,
    allowed_jids: &[AllowedJid],
) -> Result<Jid, String> {
    let default_text =
        default_recipient.ok_or_else(|| "xmpp config requires `default_recipient`".to_owned())?;
    let default_recipient =
        Jid::new(&default_text).map_err(|e| format!("invalid xmpp `default_recipient`: {e}"))?;
    if allowed_jids
        .iter()
        .any(|allowed| allowed.matches(&default_recipient))
    {
        Ok(default_recipient)
    } else {
        Err("xmpp `default_recipient` must match `allowed_jids`".to_owned())
    }
}

fn validate_muc_service(service: Option<String>) -> Result<Option<BareJid>, String> {
    service
        .map(|service| {
            let jid = Jid::new(&service).map_err(|e| format!("invalid xmpp muc.service: {e}"))?;
            if jid.node().is_some() || jid.resource().is_some() {
                return Err(
                    "xmpp `muc.service` must be a domain-only JID like `conference.example.org`"
                        .to_owned(),
                );
            }
            Ok(jid.to_bare())
        })
        .transpose()
}

fn validate_max_message_bytes(max_message_bytes: Option<usize>) -> Result<usize, String> {
    let max_message_bytes = max_message_bytes.unwrap_or(DEFAULT_MESSAGE_LIMIT);
    if max_message_bytes == 0 {
        return Err("xmpp `max_message_bytes` must be greater than zero".to_owned());
    }
    if max_message_bytes > MAX_MESSAGE_LIMIT {
        return Err(format!(
            "xmpp `max_message_bytes` must be at most {MAX_MESSAGE_LIMIT}"
        ));
    }
    Ok(max_message_bytes)
}

fn validate_resource_prefix(resource_prefix: Option<String>) -> String {
    clean_token_or(
        resource_prefix
            .as_deref()
            .unwrap_or(DEFAULT_RESOURCE_PREFIX),
        DEFAULT_RESOURCE_PREFIX,
    )
}

fn validate_room_prefix(room_prefix: Option<String>) -> String {
    clean_token_or(
        room_prefix.as_deref().unwrap_or(DEFAULT_ROOM_PREFIX),
        DEFAULT_ROOM_PREFIX,
    )
}

#[derive(Default)]
struct State {
    /// Validated runtime config.
    config: Option<RuntimeConfig>,
    /// Agents currently registered with the bridge.
    registered_agents: HashSet<AgentId>,
    /// XMPP conversation address per agent.
    conversations: HashMap<AgentId, String>,
    /// Current Tau session id used for stable per-session room names.
    current_session_id: Option<SessionId>,
    /// Whether the XMPP bridge has been started.
    bridge_started: bool,
}

struct Extension {
    /// Shared runtime state.
    state: Arc<Mutex<State>>,
    /// XMPP bridge implementation.
    bridge: Arc<dyn XmppBridge>,
    /// Output channel toward the harness.
    output: Output,
    /// Shared shutdown signal.
    shutdown: Arc<ShutdownSignal>,
}

impl Extension {
    fn new(bridge: Arc<dyn XmppBridge>, output: impl Into<Output>) -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            bridge,
            output: output.into(),
            shutdown: Arc::new(ShutdownSignal::new()),
        }
    }

    fn apply_config(&self, cfg: RuntimeConfig) -> Result<(), String> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.bridge_started {
            return Err(immutable_config_error());
        }
        state.config = Some(cfg);
        Ok(())
    }

    fn config_is_locked(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .bridge_started
    }

    fn clear_config_before_start(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.bridge_started {
            state.config = None;
            state.registered_agents.clear();
            state.conversations.clear();
        }
    }

    fn dispatch_tool(&self, invoke: ToolStarted) {
        self.output.emit(Event::ToolProgress(ToolProgress {
            call_id: invoke.call_id.clone(),
            tool_name: invoke.tool_name.clone(),
            message: Some("xmpp tool started".to_owned()),
            progress: None,
            display: Some(ToolUseState {
                status: ToolUseStatus::InProgress,
                status_text: tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
                ..Default::default()
            }),
        }));
        let event = match invoke.tool_name.as_str() {
            REGISTER_TOOL_NAME => self.handle_register(invoke),
            SEND_TOOL_NAME => self.handle_send(invoke),
            _ => tool_error(invoke, "unknown xmpp tool".to_owned()),
        };
        self.output.emit(event);
    }

    fn handle_register(&self, invoke: ToolStarted) -> Event {
        if let Err(message) = cbor_reject_unknown_fields(&invoke.arguments, &["enabled"]) {
            return tool_error(invoke, message);
        }
        let enabled = match cbor_bool_field(&invoke.arguments, "enabled") {
            Ok(enabled) => enabled,
            Err(message) => return tool_error(invoke, message),
        };
        if enabled {
            let cfg = {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                let Some(cfg) = state.config.clone() else {
                    return tool_error(invoke, "xmpp extension is not configured".to_owned());
                };
                if state.current_session_id.is_none() {
                    return tool_error(
                        invoke,
                        "xmpp_register requires an active Tau session; no session_started event has been observed yet".to_owned(),
                    );
                }
                if !state.bridge_started {
                    if let Err(message) = self.bridge.ensure_started(
                        cfg.clone(),
                        self.output.clone(),
                        Arc::clone(&self.shutdown),
                    ) {
                        return tool_error(invoke, message);
                    }
                    state.bridge_started = true;
                }
                cfg
            };
            if let Err(message) = self.bridge.wait_until_ready(ONLINE_WAIT_TIMEOUT) {
                return tool_error(invoke, message);
            }
            let session_id = {
                let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                let Some(session_id) = state.current_session_id.clone() else {
                    return tool_error(
                        invoke,
                        "xmpp_register requires an active Tau session; no session_started event has been observed yet".to_owned(),
                    );
                };
                session_id
            };
            let address = match self
                .bridge
                .register_agent(&cfg, &session_id, &invoke.agent_id)
            {
                Ok(address) => address,
                Err(message) => return tool_error(invoke, message),
            };
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.registered_agents.insert(invoke.agent_id.clone());
            state
                .conversations
                .insert(invoke.agent_id.clone(), address.clone());
            tool_result(
                invoke,
                &format!(
                    "registered for XMPP messages at {address}. Plaintext over TLS only; no OMEMO/E2EE."
                ),
            )
        } else {
            let _ = self.bridge.unregister_agent(&invoke.agent_id);
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.registered_agents.remove(&invoke.agent_id);
            state.conversations.remove(&invoke.agent_id);
            tool_result(invoke, "unregistered from XMPP messages")
        }
    }

    fn handle_send(&self, invoke: ToolStarted) -> Event {
        if let Err(message) = cbor_reject_unknown_fields(&invoke.arguments, &["message"]) {
            return tool_error(invoke, message);
        }
        let message = match cbor_string_field(&invoke.arguments, "message") {
            Ok(message) => message,
            Err(message) => return tool_error(invoke, message),
        };
        if message.trim().is_empty() {
            return tool_error(invoke, "`message` must not be empty".to_owned());
        }
        {
            let (has_config, bridge_started) = {
                let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                (state.config.is_some(), state.bridge_started)
            };
            if !has_config {
                return tool_error(invoke, "xmpp extension is not configured".to_owned());
            }
            // Tool-side readiness gives callers a clear bounded wait/error before
            // normal validation; worker-side readiness below still protects
            // against reconnect races or callers that bypass this preflight.
            if bridge_started
                && let Err(message) = self.bridge.wait_until_ready(ONLINE_WAIT_TIMEOUT)
            {
                return tool_error(invoke, message);
            }
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let Some(cfg) = state.config.as_ref() else {
                return tool_error(invoke, "xmpp extension is not configured".to_owned());
            };
            if message.len() > cfg.max_message_bytes {
                return tool_error(
                    invoke,
                    "`message` exceeds xmpp max_message_bytes".to_owned(),
                );
            }
            if !state.registered_agents.contains(&invoke.agent_id) {
                return tool_error(
                    invoke,
                    "xmpp_send requires xmpp_register(enabled: true) first".to_owned(),
                );
            }
        }
        let text = format!("[{}] {message}", invoke.agent_id.as_ref());
        match self.bridge.send_message(&invoke.agent_id, &text) {
            Ok(()) => tool_result(invoke, "sent XMPP message"),
            Err(message) => tool_error(invoke, message),
        }
    }
}

impl Drop for Extension {
    fn drop(&mut self) {
        self.shutdown.request();
        if let Err(error) = self.bridge.shutdown(SHUTDOWN_TIMEOUT) {
            tracing::warn!(target: LOG_TARGET, %error, "xmpp bridge shutdown did not finish cleanly");
        }
    }
}

/// Live tokio-xmpp bridge.
#[derive(Default)]
struct LiveXmppBridge {
    /// Command channel to the XMPP worker.
    command_tx: Mutex<Option<mpsc::Sender<XmppCommand>>>,
    /// Running worker thread and completion notification.
    worker: Mutex<Option<XmppWorkerThread>>,
    /// Shared shutdown signal used by the running worker.
    shutdown: Mutex<Option<Arc<ShutdownSignal>>>,
}

struct XmppWorkerThread {
    /// Worker thread handle.
    join: JoinHandle<()>,
    /// Notification sent after the worker thread exits.
    done_rx: mpsc::Receiver<()>,
}

enum XmppCommand {
    Register {
        /// Tau session containing the agent.
        session_id: SessionId,
        /// Agent to register.
        agent_id: AgentId,
        /// Response channel carrying the conversation address.
        response: mpsc::Sender<Result<String, String>>,
    },
    Unregister {
        /// Agent to unregister.
        agent_id: AgentId,
        /// Response channel after leave/unregister cleanup has been attempted.
        response: mpsc::Sender<()>,
    },
    Send {
        /// Sending agent.
        agent_id: AgentId,
        /// Text body to send.
        text: String,
        /// Response channel.
        response: mpsc::Sender<Result<(), String>>,
    },
    WaitReady {
        /// Maximum time to wait for an authenticated online stream.
        timeout: Duration,
        /// Response channel.
        response: mpsc::Sender<Result<(), String>>,
    },
}

impl XmppBridge for LiveXmppBridge {
    fn ensure_started(
        &self,
        cfg: RuntimeConfig,
        output: Output,
        shutdown: Arc<ShutdownSignal>,
    ) -> Result<(), String> {
        let mut guard = self.command_tx.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_some() {
            return Ok(());
        }
        let (command_tx, command_rx) = mpsc::channel();
        let worker_tx = command_tx.clone();
        let (done_tx, done_rx) = mpsc::channel();
        let worker_shutdown = Arc::clone(&shutdown);
        let join = std::thread::Builder::new()
            .name("tau-ext-xmpp".to_owned())
            .spawn(move || {
                xmpp_thread(cfg, command_rx, output, worker_shutdown);
                let _ = done_tx.send(());
            })
            .map_err(|e| format!("failed to spawn xmpp worker: {e}"))?;
        *guard = Some(worker_tx);
        *self.shutdown.lock().unwrap_or_else(|e| e.into_inner()) = Some(shutdown);
        *self.worker.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(XmppWorkerThread { join, done_rx });
        Ok(())
    }

    fn register_agent(
        &self,
        _cfg: &RuntimeConfig,
        session_id: &SessionId,
        agent_id: &AgentId,
    ) -> Result<String, String> {
        let tx = self.command_sender()?;
        let (response_tx, response_rx) = mpsc::channel();
        tx.send(XmppCommand::Register {
            session_id: session_id.clone(),
            agent_id: agent_id.clone(),
            response: response_tx,
        })
        .map_err(|_| "xmpp worker is not running".to_owned())?;
        response_rx
            .recv_timeout(COMMAND_TIMEOUT)
            .map_err(|_| "timed out waiting for xmpp registration".to_owned())?
    }

    fn unregister_agent(&self, agent_id: &AgentId) -> Result<(), String> {
        let tx = self.command_sender()?;
        let (response_tx, response_rx) = mpsc::channel();
        tx.send(XmppCommand::Unregister {
            agent_id: agent_id.clone(),
            response: response_tx,
        })
        .map_err(|_| "xmpp worker is not running".to_owned())?;
        response_rx
            .recv_timeout(COMMAND_TIMEOUT)
            .map_err(|_| "timed out waiting for xmpp unregister".to_owned())
    }

    fn wait_until_ready(&self, timeout: Duration) -> Result<(), String> {
        let tx = self.command_sender()?;
        let (response_tx, response_rx) = mpsc::channel();
        tx.send(XmppCommand::WaitReady {
            timeout,
            response: response_tx,
        })
        .map_err(|_| "xmpp worker is not running".to_owned())?;
        response_rx
            .recv_timeout(timeout + READY_RESPONSE_SLACK)
            .map_err(|_| "timed out waiting for xmpp readiness".to_owned())?
    }

    fn send_message(&self, agent_id: &AgentId, text: &str) -> Result<(), String> {
        let tx = self.command_sender()?;
        let (response_tx, response_rx) = mpsc::channel();
        tx.send(XmppCommand::Send {
            agent_id: agent_id.clone(),
            text: text.to_owned(),
            response: response_tx,
        })
        .map_err(|_| "xmpp worker is not running".to_owned())?;
        response_rx
            .recv_timeout(COMMAND_TIMEOUT)
            .map_err(|_| "timed out waiting for xmpp send".to_owned())?
    }

    fn shutdown(&self, timeout: Duration) -> Result<(), String> {
        if let Some(shutdown) = self
            .shutdown
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            shutdown.request();
        }
        self.command_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let mut worker = self.worker.lock().unwrap_or_else(|e| e.into_inner());
        let Some(worker_ref) = worker.as_ref() else {
            return Ok(());
        };
        worker_ref
            .done_rx
            .recv_timeout(timeout)
            .map_err(|_| "timed out waiting for xmpp worker shutdown".to_owned())?;
        let worker = worker.take().expect("worker checked above");
        worker
            .join
            .join()
            .map_err(|_| "xmpp worker thread panicked during shutdown".to_owned())
    }
}

impl LiveXmppBridge {
    /// Return the active worker command channel.
    fn command_sender(&self) -> Result<mpsc::Sender<XmppCommand>, String> {
        self.command_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| "xmpp bridge is not started".to_owned())
    }
}

fn xmpp_thread(
    cfg: RuntimeConfig,
    command_rx: mpsc::Receiver<XmppCommand>,
    output: Output,
    shutdown: Arc<ShutdownSignal>,
) {
    match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(xmpp_worker(cfg, command_rx, output, shutdown)),
        Err(error) => tracing::warn!(target: LOG_TARGET, %error, "failed to create xmpp runtime"),
    }
}

async fn xmpp_worker(
    cfg: RuntimeConfig,
    command_rx: mpsc::Receiver<XmppCommand>,
    output: Output,
    shutdown: Arc<ShutdownSignal>,
) {
    if let Err(error) = tokio_xmpp::rustls::crypto::ring::default_provider().install_default() {
        tracing::debug!(target: LOG_TARGET, ?error, "rustls provider was already installed or unavailable");
    }
    let resource = generated_resource(&cfg);
    let login_jid = match Jid::new(&format!("{}/{resource}", cfg.account_jid)) {
        Ok(jid) => jid,
        Err(error) => {
            tracing::warn!(target: LOG_TARGET, %error, "failed to build xmpp resource jid");
            return;
        }
    };
    let mut client = Client::new(login_jid, cfg.password.clone());
    let mut command_rx = std_to_tokio(command_rx);
    let mut worker = WorkerState::new(cfg, output, Arc::clone(&shutdown));
    loop {
        if shutdown.is_requested() {
            worker
                .leave_all_with_timeout(&mut client, WORKER_SHUTDOWN_CLEANUP_TIMEOUT)
                .await;
            return;
        }
        tokio::select! {
            event = client.next() => {
                let Some(event) = event else {
                    worker
                        .leave_all_with_timeout(&mut client, WORKER_SHUTDOWN_CLEANUP_TIMEOUT)
                        .await;
                    return;
                };
                match event {
                    tokio_xmpp::Event::Online { bound_jid, .. } => {
                        if run_until_worker_shutdown(
                            Arc::clone(&shutdown),
                            worker.handle_online(bound_jid, &mut client),
                        )
                        .await
                        == WorkerRunOutcome::Shutdown
                        {
                            worker
                                .leave_all_with_timeout(&mut client, WORKER_SHUTDOWN_CLEANUP_TIMEOUT)
                                .await;
                            return;
                        }
                    }
                    tokio_xmpp::Event::Disconnected(error) => {
                        tracing::warn!(target: LOG_TARGET, %error, "xmpp disconnected");
                        worker.handle_disconnected();
                    }
                    tokio_xmpp::Event::Stanza(stanza) => worker.handle_stanza(stanza),
                }
            }
            command = command_rx.recv() => {
                let Some(command) = command else {
                    worker
                        .leave_all_with_timeout(&mut client, WORKER_SHUTDOWN_CLEANUP_TIMEOUT)
                        .await;
                    return;
                };
                if run_until_worker_shutdown(
                    Arc::clone(&shutdown),
                    worker.handle_command(command, &mut client),
                )
                .await
                == WorkerRunOutcome::Shutdown
                {
                    worker
                        .leave_all_with_timeout(&mut client, WORKER_SHUTDOWN_CLEANUP_TIMEOUT)
                        .await;
                    return;
                }
            }
            () = wait_for_shutdown(Arc::clone(&shutdown)) => {
                worker
                    .leave_all_with_timeout(&mut client, WORKER_SHUTDOWN_CLEANUP_TIMEOUT)
                    .await;
                return;
            }
        }
    }
}

fn std_to_tokio<T: Send + 'static>(
    rx: mpsc::Receiver<T>,
) -> tokio::sync::mpsc::UnboundedReceiver<T> {
    let (tx, tokio_rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(item) = rx.recv() {
            if tx.send(item).is_err() {
                break;
            }
        }
    });
    tokio_rx
}

async fn wait_for_shutdown(shutdown: Arc<ShutdownSignal>) {
    shutdown.wait().await;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkerRunOutcome {
    /// The worker operation finished before shutdown was requested.
    Completed,
    /// Shutdown was requested before the worker operation finished.
    Shutdown,
}

async fn run_until_worker_shutdown<F>(
    shutdown: Arc<ShutdownSignal>,
    operation: F,
) -> WorkerRunOutcome
where
    F: std::future::Future<Output = ()>,
{
    tokio::select! {
        () = operation => WorkerRunOutcome::Completed,
        () = wait_for_shutdown(shutdown) => WorkerRunOutcome::Shutdown,
    }
}

struct WorkerState {
    /// Runtime config.
    cfg: RuntimeConfig,
    /// Output channel toward the harness.
    output: Output,
    /// Shared shutdown signal used to cancel long best-effort operations.
    shutdown: Arc<ShutdownSignal>,
    /// Server-returned bound JID.
    bound_jid: Option<Jid>,
    /// Registered conversations.
    conversations: HashMap<AgentId, Conversation>,
    /// MUC joins that have been sent but are not yet routable conversations.
    pending_muc_joins: HashMap<AgentId, MucOccupant>,
    /// MUC room to agent mapping.
    room_to_agent: HashMap<BareJid, AgentId>,
    /// MUC occupant real JID cache.
    occupant_real_jids: HashMap<Jid, Jid>,
}

impl WorkerState {
    /// Create a worker state.
    fn new(cfg: RuntimeConfig, output: impl Into<Output>, shutdown: Arc<ShutdownSignal>) -> Self {
        Self {
            cfg,
            output: output.into(),
            shutdown,
            bound_jid: None,
            conversations: HashMap::new(),
            pending_muc_joins: HashMap::new(),
            room_to_agent: HashMap::new(),
            occupant_real_jids: HashMap::new(),
        }
    }

    /// Process one command from tool handlers.
    async fn handle_command(&mut self, command: XmppCommand, client: &mut Client) {
        match command {
            XmppCommand::Register {
                session_id,
                agent_id,
                response,
            } => {
                let result = match tokio::time::timeout(
                    REGISTER_TIMEOUT,
                    self.register_agent(session_id, agent_id.clone(), client),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => {
                        self.unregister_agent(&agent_id, client).await;
                        Err("timed out registering xmpp conversation".to_owned())
                    }
                };
                if self
                    .finish_register_response(&agent_id, result, response, client)
                    .await
                {
                    self.send_post_register_notice(&agent_id, client).await;
                }
            }
            XmppCommand::Unregister { agent_id, response } => {
                self.unregister_agent(&agent_id, client).await;
                let _ = response.send(());
            }
            XmppCommand::Send {
                agent_id,
                text,
                response,
            } => {
                let result = self.send_message(&agent_id, &text, client).await;
                let _ = response.send(result);
            }
            XmppCommand::WaitReady { timeout, response } => {
                let result = self.ensure_online_with_timeout(client, timeout).await;
                let _ = response.send(result);
            }
        }
    }

    /// Send a register response, roll back worker routing if the caller has
    /// already timed out and dropped its receiver, and return whether the
    /// registration remains active.
    async fn finish_register_response(
        &mut self,
        agent_id: &AgentId,
        result: Result<String, String>,
        response: mpsc::Sender<Result<String, String>>,
        client: &mut Client,
    ) -> bool {
        let registered = result.is_ok();
        if response.send(result).is_err() && registered {
            self.unregister_agent(agent_id, client).await;
            return false;
        }
        registered
    }

    /// Register one agent conversation.
    async fn register_agent(
        &mut self,
        session_id: SessionId,
        agent_id: AgentId,
        client: &mut Client,
    ) -> Result<String, String> {
        if let Some(conversation) = self.conversations.get(&agent_id) {
            return Ok(conversation.address());
        }
        let conversation = match self.cfg.routing_mode {
            RoutingMode::Muc => {
                self.ensure_online(client).await?;
                let room = self.muc_room_for(&session_id, &agent_id)?;
                self.ensure_muc_room_available(&room, &agent_id)?;
                let nick = format!("{}-{}", self.cfg.resource_prefix, short_random_hex());
                let occupant = MucOccupant::new(room.clone(), nick);
                self.pending_muc_joins
                    .insert(agent_id.clone(), occupant.clone());
                if let Err(error) = join_room(client, &occupant.room, &occupant.nick).await {
                    self.leave_pending_muc_join(&agent_id, client).await;
                    return Err(error);
                }
                if let Err(error) = self.setup_joined_muc_room(client, &occupant).await {
                    self.leave_pending_muc_join(&agent_id, client).await;
                    return Err(error);
                }
                self.room_to_agent.insert(room.clone(), agent_id.clone());
                let conversation = Conversation::Muc {
                    room: occupant.room.clone(),
                    nick: occupant.nick.clone(),
                };
                self.pending_muc_joins.remove(&agent_id);
                self.conversations
                    .insert(agent_id.clone(), conversation.clone());
                conversation
            }
            RoutingMode::DirectResource => {
                if self
                    .conversations
                    .values()
                    .any(|conversation| matches!(conversation, Conversation::Direct { .. }))
                {
                    return Err("direct_resource mode supports only one registered agent per extension instance; use routing.mode `muc` for multiple Tau agents or separate conversations".to_owned());
                }
                self.ensure_online(client).await?;
                let bound = self
                    .bound_jid
                    .clone()
                    .ok_or_else(|| "xmpp connection is not online yet".to_owned())?;
                let notice = format!(
                    "Tau agent {} is available at {} (plaintext over TLS; no OMEMO/E2EE).",
                    agent_id.as_ref(),
                    bound
                );
                send_chat(client, self.cfg.default_recipient.clone(), &notice).await?;
                Conversation::Direct { full_jid: bound }
            }
        };
        let address = conversation.address();
        self.conversations.entry(agent_id).or_insert(conversation);
        Ok(address)
    }

    /// Send best-effort human notices after the registration response has
    /// already been returned to the tool caller.
    async fn send_post_register_notice(&self, agent_id: &AgentId, client: &mut Client) {
        if !self.cfg.muc.invite_default_recipient || self.shutdown_requested() {
            return;
        }
        let Some(Conversation::Muc { room, .. }) = self.conversations.get(agent_id).cloned() else {
            return;
        };
        let invite_reason = format!(
            "Tau agent {} registered this private room (plaintext over TLS; no OMEMO/E2EE).",
            agent_id.as_ref()
        );
        let invite_status = match self
            .until_shutdown(send_muc_invite(
                client,
                room.clone(),
                self.cfg.default_recipient.clone(),
                &invite_reason,
            ))
            .await
        {
            Ok(()) => "sent a MUC invite for",
            Err(error) => {
                tracing::warn!(target: LOG_TARGET, %error, room = %room, "failed to send xmpp muc invite; sending direct diagnostic notice");
                "could not send a MUC invite for"
            }
        };
        if self.shutdown_requested() {
            return;
        }
        let notice = format!(
            "Tau agent {} {} room {}. If your client did not show the invite, join this room manually; replies to this direct notice are not routed in MUC mode. Plaintext over TLS; no OMEMO/E2EE.",
            agent_id.as_ref(),
            invite_status,
            room
        );
        if let Err(error) = self
            .until_shutdown(send_chat(
                client,
                self.cfg.default_recipient.clone(),
                &notice,
            ))
            .await
        {
            tracing::warn!(target: LOG_TARGET, %error, room = %room, "failed to send xmpp muc fallback notice after join");
        }
    }

    /// Return whether shutdown has been requested.
    fn shutdown_requested(&self) -> bool {
        self.shutdown.is_requested()
    }

    /// Run a best-effort operation only while shutdown has not been requested.
    async fn until_shutdown<F, T>(&self, operation: F) -> Result<T, String>
    where
        F: std::future::Future<Output = Result<T, String>>,
    {
        tokio::select! {
            result = operation => result,
            () = wait_for_shutdown(Arc::clone(&self.shutdown)) => {
                Err("xmpp shutdown requested".to_owned())
            }
        }
    }

    /// Build the stable MUC room JID for a Tau session and agent pair.
    fn muc_room_for(&self, session_id: &SessionId, agent_id: &AgentId) -> Result<BareJid, String> {
        let service = self
            .cfg
            .muc
            .service
            .clone()
            .ok_or_else(|| "xmpp muc.service is not configured".to_owned())?;
        Jid::new(&format!(
            "{}-{}@{}",
            self.cfg.muc.room_prefix,
            muc_room_label(session_id, agent_id),
            service.domain()
        ))
        .map_err(|e| format!("failed to build muc room jid: {e}"))
        .map(|jid| jid.to_bare())
    }

    /// Fail closed if a generated MUC room is already owned by another agent.
    fn ensure_muc_room_available(&self, room: &BareJid, agent_id: &AgentId) -> Result<(), String> {
        if let Some(existing) = self.room_to_agent.get(room)
            && existing != agent_id
        {
            return Err(format!(
                "generated xmpp muc room collision for {room}; refusing to overwrite routing from agent {} to agent {}",
                existing.as_ref(),
                agent_id.as_ref()
            ));
        }
        if self
            .pending_muc_joins
            .iter()
            .any(|(pending_agent, occupant)| pending_agent != agent_id && &occupant.room == room)
        {
            return Err(format!(
                "generated xmpp muc room collision for {room}; another agent is already joining this room"
            ));
        }
        Ok(())
    }

    /// Wait up to the standard readiness timeout for the XMPP stream to
    /// become online and process any intervening stanzas needed to keep routing
    /// state fresh.
    async fn ensure_online(&mut self, client: &mut Client) -> Result<(), String> {
        self.ensure_online_with_timeout(client, ONLINE_WAIT_TIMEOUT)
            .await
    }

    /// Wait for the XMPP stream to become online within a caller-selected
    /// bound.
    async fn ensure_online_with_timeout(
        &mut self,
        client: &mut Client,
        timeout: Duration,
    ) -> Result<(), String> {
        if self.bound_jid.is_some() {
            return Ok(());
        }
        let wait = async {
            loop {
                let Some(event) = client.next().await else {
                    return Err("xmpp connection ended before becoming online".to_owned());
                };
                match event {
                    tokio_xmpp::Event::Online { bound_jid, .. } => {
                        self.handle_online(bound_jid, client).await;
                        return Ok(());
                    }
                    tokio_xmpp::Event::Disconnected(error) => {
                        tracing::warn!(target: LOG_TARGET, %error, "xmpp disconnected while waiting for online state");
                    }
                    tokio_xmpp::Event::Stanza(stanza) => self.handle_stanza(stanza),
                }
            }
        };
        tokio::time::timeout(timeout, wait)
            .await
            .map_err(|_| {
                format!(
                    "xmpp connection did not become online within {}s; retry after the account connects",
                    timeout.as_secs()
                )
            })?
    }

    /// Mark the stream offline after a disconnect event so the next command
    /// waits for a fresh authenticated `Online` event before using the
    /// connection.
    fn handle_disconnected(&mut self) {
        self.bound_jid = None;
        self.occupant_real_jids.clear();
    }

    /// Unregister one agent and leave its MUC room when applicable.
    async fn unregister_agent(&mut self, agent_id: &AgentId, client: &mut Client) {
        self.leave_pending_muc_join(agent_id, client).await;
        if let Some(conversation) = self.conversations.get(agent_id).cloned() {
            self.leave_conversation(&conversation, client).await;
            self.remove_conversation(agent_id);
        }
    }

    /// Remove one registered conversation and its room mapping.
    fn remove_conversation(&mut self, agent_id: &AgentId) -> Option<Conversation> {
        let conversation = self.conversations.remove(agent_id);
        self.pending_muc_joins.remove(agent_id);
        self.room_to_agent.retain(|_, mapped| mapped != agent_id);
        conversation
    }

    /// Leave all registered MUC conversations before worker shutdown, bounded
    /// by one overall cleanup budget.
    async fn leave_all_with_timeout(&mut self, client: &mut Client, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        let conversations = self
            .conversations
            .drain()
            .map(|(_, conv)| conv)
            .collect::<Vec<_>>();
        for conversation in conversations {
            self.leave_conversation_until(&conversation, client, deadline)
                .await;
        }
        let pending = self
            .pending_muc_joins
            .drain()
            .map(|(_, occupant)| occupant)
            .collect::<Vec<_>>();
        for occupant in pending {
            self.leave_muc_occupant_until(&occupant, client, deadline)
                .await;
        }
        self.room_to_agent.clear();
        self.occupant_real_jids.clear();
    }

    /// Send leave presence for a MUC conversation. Direct conversations require
    /// no per-conversation unavailable stanza.
    async fn leave_conversation(&self, conversation: &Conversation, client: &mut Client) {
        if let Conversation::Muc { room, nick } = conversation
            && let Err(error) = leave_room(client, room, nick).await
        {
            tracing::warn!(target: LOG_TARGET, %error, room = %room, "failed to leave xmpp muc room");
        }
    }

    /// Send leave presence for a MUC conversation within a shutdown deadline.
    async fn leave_conversation_until(
        &self,
        conversation: &Conversation,
        client: &mut Client,
        deadline: tokio::time::Instant,
    ) {
        if let Conversation::Muc { room, nick } = conversation
            && let Err(error) = leave_room_until(client, room, nick, deadline).await
        {
            tracing::warn!(target: LOG_TARGET, %error, room = %room, "failed to leave xmpp muc room during shutdown");
        }
    }

    /// Leave a pending MUC join and remove its non-routable registration state.
    async fn leave_pending_muc_join(&mut self, agent_id: &AgentId, client: &mut Client) {
        if let Some(occupant) = self.pending_muc_joins.get(agent_id).cloned() {
            self.leave_muc_occupant(&occupant, client).await;
            self.pending_muc_joins.remove(agent_id);
        }
    }

    /// Send unavailable presence for one MUC room/nick pair.
    async fn leave_muc_occupant(&self, occupant: &MucOccupant, client: &mut Client) {
        if let Err(error) = leave_room(client, &occupant.room, &occupant.nick).await {
            tracing::warn!(target: LOG_TARGET, %error, room = %occupant.room, "failed to leave pending xmpp muc room");
        }
    }

    /// Send unavailable presence for one MUC room/nick pair within a shutdown
    /// deadline.
    async fn leave_muc_occupant_until(
        &self,
        occupant: &MucOccupant,
        client: &mut Client,
        deadline: tokio::time::Instant,
    ) {
        if let Err(error) = leave_room_until(client, &occupant.room, &occupant.nick, deadline).await
        {
            tracing::warn!(target: LOG_TARGET, %error, room = %occupant.room, "failed to leave pending xmpp muc room during shutdown");
        }
    }

    /// Refresh connection-dependent state after the XMPP stream comes online.
    async fn handle_online(&mut self, bound_jid: Jid, client: &mut Client) {
        self.refresh_online_state(bound_jid, client).await;
        self.rejoin_all(client).await;
    }

    /// Refresh online state without recursively rejoining rooms.
    async fn refresh_online_state(&mut self, bound_jid: Jid, client: &mut Client) {
        let direct_updates = self.apply_online_state(bound_jid);
        if let Err(error) = send_presence(client, Presence::available().with_priority(-1)).await {
            tracing::warn!(target: LOG_TARGET, %error, "failed to send xmpp available presence");
        }
        self.notify_direct_reconnects(direct_updates, client).await;
    }

    /// Apply state changes for a newly online stream and return direct-resource
    /// registrations whose externally visible address changed.
    fn apply_online_state(&mut self, bound_jid: Jid) -> Vec<(AgentId, Jid)> {
        self.bound_jid = Some(bound_jid.clone());
        self.occupant_real_jids.clear();
        self.update_direct_conversations(bound_jid)
    }

    /// Update direct-resource conversations after reconnect/resource changes.
    fn update_direct_conversations(&mut self, bound_jid: Jid) -> Vec<(AgentId, Jid)> {
        let mut updates = Vec::new();
        for (agent_id, conversation) in &mut self.conversations {
            let Conversation::Direct { full_jid } = conversation else {
                continue;
            };
            if full_jid == &bound_jid {
                continue;
            }
            *full_jid = bound_jid.clone();
            updates.push((agent_id.clone(), bound_jid.clone()));
        }
        updates
    }

    /// Notify the configured human recipient about changed direct-resource
    /// addresses after reconnect.
    async fn notify_direct_reconnects(&self, updates: Vec<(AgentId, Jid)>, client: &mut Client) {
        for (agent_id, bound_jid) in updates {
            let notice = format!(
                "Tau agent {} reconnected and is now available at {} (plaintext over TLS; no OMEMO/E2EE).",
                agent_id.as_ref(),
                bound_jid
            );
            if let Err(error) = send_chat(client, self.cfg.default_recipient.clone(), &notice).await
            {
                tracing::warn!(target: LOG_TARGET, %error, agent_id = %agent_id.as_ref(), "failed to send xmpp direct-resource reconnect notice");
            }
        }
    }

    /// Send one message to a registered conversation.
    async fn send_message(
        &mut self,
        agent_id: &AgentId,
        text: &str,
        client: &mut Client,
    ) -> Result<(), String> {
        self.ensure_online(client).await?;
        let conversation = self
            .conversations
            .get(agent_id)
            .ok_or_else(|| "xmpp_send requires xmpp_register(enabled: true) first".to_owned())?;
        match conversation {
            Conversation::Muc { room, .. } => {
                send_groupchat(client, room.clone().into(), text).await
            }
            Conversation::Direct { .. } => {
                send_chat(client, self.cfg.default_recipient.clone(), text).await
            }
        }
    }

    /// Complete post-join setup for a MUC occupant before it becomes routable.
    async fn setup_joined_muc_room(
        &mut self,
        client: &mut Client,
        occupant: &MucOccupant,
    ) -> Result<(), String> {
        let join = self
            .wait_for_muc_self_presence(client, &occupant.room, &occupant.nick)
            .await?;
        tracing::info!(
            target: LOG_TARGET,
            room = %occupant.room,
            nick = %occupant.nick,
            created = join.created,
            statuses = ?join.statuses,
            "joined xmpp muc room"
        );
        if join.created {
            tracing::info!(target: LOG_TARGET, room = %occupant.room, "new xmpp muc room created; submitting instant-room owner config");
            submit_instant_room_config(client, &occupant.room).await?;
            tracing::info!(target: LOG_TARGET, room = %occupant.room, "submitted xmpp muc instant-room owner config");
        }
        Ok(())
    }

    /// Wait for the post-join self-presence or matching presence error for a
    /// specific room occupant JID.
    async fn wait_for_muc_self_presence(
        &mut self,
        client: &mut Client,
        room: &BareJid,
        nick: &str,
    ) -> Result<MucJoin, String> {
        let occupant = muc_occupant_jid(room, nick)?;
        let wait = async {
            loop {
                let Some(event) = client.next().await else {
                    return Err(format!(
                        "xmpp connection ended while waiting for MUC self-presence from {occupant}"
                    ));
                };
                match event {
                    tokio_xmpp::Event::Online { bound_jid, .. } => {
                        self.refresh_online_state(bound_jid, client).await;
                    }
                    tokio_xmpp::Event::Disconnected(error) => {
                        self.handle_disconnected();
                        tracing::warn!(target: LOG_TARGET, %error, room = %room, nick = %nick, "xmpp disconnected while waiting for muc join confirmation");
                    }
                    tokio_xmpp::Event::Stanza(Stanza::Presence(presence))
                        if muc_presence_from(&presence, &occupant) =>
                    {
                        let join = MucJoin::from_self_presence(&presence)?;
                        self.handle_presence(presence);
                        return Ok(join);
                    }
                    tokio_xmpp::Event::Stanza(stanza) => self.handle_stanza(stanza),
                }
            }
        };
        tokio::time::timeout(STANZA_TIMEOUT, wait)
            .await
            .map_err(|_| {
                format!(
                    "timed out waiting for xmpp MUC self-presence from {occupant}; room may still be locked or unusable"
                )
            })?
    }

    /// Rejoin all known MUC rooms after an online/reconnect event.
    async fn rejoin_all(&mut self, client: &mut Client) {
        for (room, nick) in self.muc_rooms_to_rejoin() {
            let occupant = MucOccupant::new(room, nick);
            if let Err(error) = join_room(client, &occupant.room, &occupant.nick).await {
                tracing::warn!(target: LOG_TARGET, %error, room = %occupant.room, "failed to rejoin xmpp muc room");
                continue;
            }
            if let Err(error) = self.setup_joined_muc_room(client, &occupant).await {
                tracing::warn!(target: LOG_TARGET, %error, room = %occupant.room, "failed to confirm/setup rejoined xmpp muc room");
            }
        }
    }

    /// Return MUC rooms that should be rejoined after reconnect.
    fn muc_rooms_to_rejoin(&self) -> Vec<(BareJid, String)> {
        self.conversations
            .values()
            .filter_map(|conversation| match conversation {
                Conversation::Muc { room, nick } => Some((room.clone(), nick.clone())),
                Conversation::Direct { .. } => None,
            })
            .collect()
    }

    /// Process an inbound stanza.
    fn handle_stanza(&mut self, stanza: Stanza) {
        match stanza {
            Stanza::Message(message) => self.handle_message(message),
            Stanza::Presence(presence) => self.handle_presence(presence),
            Stanza::Iq(iq) => self.handle_iq(iq),
        }
    }

    /// Process inbound IQ stanzas not already claimed by an explicit IQ
    /// response token.
    fn handle_iq(&self, iq: Iq) {
        if let Iq::Error {
            from, id, error, ..
        } = iq
        {
            tracing::warn!(target: LOG_TARGET, from = ?from, id = %id, error = ?error, "received xmpp iq error");
        }
    }

    /// Process inbound presence for MUC real-JID allowlist enforcement.
    fn handle_presence(&mut self, presence: Presence) {
        let Some(from) = presence.from.clone() else {
            return;
        };
        if matches!(
            presence.type_,
            PresenceType::Unavailable | PresenceType::Error
        ) {
            self.occupant_real_jids.remove(&from);
            if presence.type_ == PresenceType::Error {
                let error = presence
                    .payloads
                    .iter()
                    .find_map(|payload| StanzaError::try_from(payload.clone()).ok());
                tracing::warn!(target: LOG_TARGET, from = %from, error = ?error, "received xmpp presence error");
            }
            return;
        }
        for payload in presence.payloads {
            if let Ok(muc_user) = MucUser::try_from(payload)
                && let Some(real_jid) = muc_user.items.iter().find_map(|item| item.jid.clone())
            {
                self.occupant_real_jids
                    .insert(from.clone(), real_jid.into());
            }
        }
    }

    /// Process inbound message stanzas.
    fn handle_message(&mut self, message: Message) {
        if message.type_ == MessageType::Error {
            let error = message
                .payloads
                .iter()
                .find_map(|payload| StanzaError::try_from(payload.clone()).ok());
            tracing::warn!(target: LOG_TARGET, from = ?message.from, error = ?error, "received xmpp message error");
            return;
        }
        // XEP-0203 delayed delivery marks backlog/history. The MVP is live-only,
        // so delayed messages must not become fresh Tau prompt submissions.
        if has_delay_payload(&message) {
            return;
        }
        let Some(body) = message
            .get_best_body(Vec::new())
            .map(|(_, body)| body.trim().to_owned())
            .filter(|body| !body.is_empty())
        else {
            return;
        };
        if body.len() > self.cfg.max_message_bytes {
            return;
        }
        match message.type_ {
            MessageType::Groupchat => self.handle_groupchat(message, body),
            MessageType::Chat | MessageType::Normal => self.handle_direct(message, body),
            MessageType::Error => unreachable!("message errors returned before body handling"),
            MessageType::Headline => {}
        }
    }

    /// Process inbound MUC groupchat.
    fn handle_groupchat(&mut self, message: Message, body: String) {
        let Some(from) = message.from.clone() else {
            return;
        };
        let room = from.to_bare();
        let Some(agent_id) = self.room_to_agent.get(&room).cloned() else {
            return;
        };
        if self.is_own_muc_message(&agent_id, &from) {
            return;
        }
        let real = self.occupant_real_jids.get(&from).cloned();
        if real.is_none() && !self.cfg.muc.trust_muc_membership {
            tracing::warn!(target: LOG_TARGET, room = %room, expose_real_jids = self.cfg.muc.expose_real_jids, "dropping muc message without real jid proof");
            return;
        }
        if let Some(real_jid) = real.as_ref()
            && !self.cfg.is_allowed(real_jid)
        {
            tracing::warn!(target: LOG_TARGET, room = %room, sender = %real_jid, "dropping muc message from non-allowlisted real jid");
            return;
        }
        self.route(agent_id, format_room_prompt(real.as_ref(), &from, &body));
    }

    /// Process inbound direct chat fallback.
    fn handle_direct(&mut self, message: Message, body: String) {
        let Some(from) = message.from.clone() else {
            return;
        };
        if !self.cfg.is_allowed(&from) {
            tracing::warn!(target: LOG_TARGET, sender = %from, "dropping direct xmpp message from non-allowlisted jid");
            return;
        }
        let Some(to) = message.to.as_ref() else {
            return;
        };
        let Some(bound) = self.bound_jid.as_ref() else {
            return;
        };
        if to != bound {
            tracing::warn!(target: LOG_TARGET, sender = %from, to = %to, bound = %bound, "dropping direct xmpp message not addressed to the current bound full jid");
            return;
        }
        let agents: Vec<_> = self
            .conversations
            .iter()
            .filter_map(|(agent, conv)| {
                matches!(conv, Conversation::Direct { .. }).then_some(agent.clone())
            })
            .collect();
        if agents.len() != 1 {
            tracing::warn!(target: LOG_TARGET, sender = %from, "dropping direct xmpp message with no registered direct-resource agent; in MUC mode, send messages in the agent room instead of replying to direct notices");
            return;
        }
        self.route(
            agents[0].clone(),
            format!(
                "[xmpp direct message from {}]: {body}",
                prompt_label(from.to_bare())
            ),
        );
    }

    /// Return whether a MUC message came from our occupant nick.
    fn is_own_muc_message(&self, agent_id: &AgentId, from: &Jid) -> bool {
        let Some(resource) = from.resource() else {
            return false;
        };
        self.conversations
            .get(agent_id)
            .is_some_and(|conversation| match conversation {
                Conversation::Muc { nick, .. } => resource.as_str() == nick,
                Conversation::Direct { .. } => false,
            })
    }

    /// Submit text to the harness prompt boundary.
    fn route(&self, agent_id: AgentId, text: String) {
        self.output
            .emit(Event::ExtPromptSubmitRequest(ExtPromptSubmitRequest {
                agent_id,
                text,
                message_class: tau_proto::PromptMessageClass::User,
                ctx_id: None,
            }));
    }
}

/// Format an inbound MUC prompt with channel and best-available source context.
///
/// Call only after MUC authorization has accepted either a verified real JID or
/// `trust_muc_membership`. Without `real`, the occupant resource is only a weak
/// room-local display label, not proof of sender identity.
fn format_room_prompt(real: Option<&Jid>, occupant: &Jid, body: &str) -> String {
    if let Some(source) = display_muc_source(real, occupant) {
        format!("[xmpp room message from {source}]: {body}")
    } else {
        format!("[xmpp room message]: {body}")
    }
}

/// Return a concise sender label for user-visible inbound MUC prompt context.
fn display_muc_source(real: Option<&Jid>, occupant: &Jid) -> Option<String> {
    if let Some(real) = real {
        return Some(prompt_label(real.to_bare()));
    }
    occupant
        .resource()
        .map(|resource| format!("occupant {}", prompt_label(resource.as_str())))
}

/// Return a single-line prompt label that cannot close the prefix bracket.
fn prompt_label(label: impl std::fmt::Display) -> String {
    label
        .to_string()
        .chars()
        .map(|ch| {
            if ch.is_control() || matches!(ch, '[' | ']') {
                ' '
            } else {
                ch
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Clone)]
enum Conversation {
    /// MUC room conversation.
    Muc {
        /// Room bare JID.
        room: BareJid,
        /// Tau occupant nick.
        nick: String,
    },
    /// Direct full-resource conversation.
    Direct {
        /// Bound full JID.
        full_jid: Jid,
    },
}

#[derive(Clone)]
struct MucOccupant {
    /// Room bare JID for a pending or active occupant.
    room: BareJid,
    /// Tau occupant nick in the room.
    nick: String,
}

impl MucOccupant {
    /// Create a room/nick pair used for MUC join, setup, and cleanup.
    fn new(room: BareJid, nick: String) -> Self {
        Self { room, nick }
    }
}

impl Conversation {
    /// Return the user-visible conversation address.
    fn address(&self) -> String {
        match self {
            Self::Muc { room, .. } => room.to_string(),
            Self::Direct { full_jid } => full_jid.to_string(),
        }
    }
}

#[derive(Debug)]
struct MucJoin {
    /// Whether the server reported XEP-0045 status 201 for a newly-created
    /// room.
    created: bool,
    /// MUC status codes included in the self-presence.
    statuses: Vec<MucStatus>,
}

impl MucJoin {
    /// Inspect the exact MUC self-presence returned after join and classify
    /// success, new-room status, or server rejection.
    fn from_self_presence(presence: &Presence) -> Result<Self, String> {
        if presence.type_ == PresenceType::Error {
            let error = presence
                .payloads
                .iter()
                .find_map(|payload| StanzaError::try_from(payload.clone()).ok());
            tracing::warn!(
                target: LOG_TARGET,
                from = ?presence.from,
                error = ?error,
                "xmpp muc join presence error"
            );
            let detail = error.map_or_else(
                || "no stanza error payload".to_owned(),
                |error| format!("{:?} {:?}", error.type_, error.defined_condition),
            );
            return Err(format!("xmpp MUC join rejected by server: {detail}"));
        }
        if presence.type_ != PresenceType::None {
            tracing::warn!(
                target: LOG_TARGET,
                from = ?presence.from,
                presence_type = ?presence.type_,
                "xmpp muc join returned non-available self-presence"
            );
            return Err(format!(
                "xmpp MUC join did not succeed; server returned {:?} self-presence",
                presence.type_
            ));
        }
        let statuses = presence
            .payloads
            .iter()
            .find_map(|payload| MucUser::try_from(payload.clone()).ok())
            .map(|muc_user| muc_user.status)
            .unwrap_or_default();
        let created = statuses.contains(&MucStatus::RoomHasBeenCreated);
        Ok(Self { created, statuses })
    }
}

async fn join_room(client: &mut Client, room: &BareJid, nick: &str) -> Result<(), String> {
    let to = muc_occupant_jid(room, nick)?;
    let presence = Presence::available()
        .with_to(to)
        .with_payload(Muc::new().with_history(History::new().with_maxstanzas(0)));
    send_presence(client, presence)
        .await
        .map_err(|e| format!("failed to join xmpp muc room: {e}"))
}

async fn leave_room(client: &mut Client, room: &BareJid, nick: &str) -> Result<(), String> {
    let presence = leave_presence(room, nick)?;
    send_presence(client, presence)
        .await
        .map_err(|e| format!("failed to leave xmpp muc room: {e}"))
}

async fn leave_room_until(
    client: &mut Client,
    room: &BareJid,
    nick: &str,
    deadline: tokio::time::Instant,
) -> Result<(), String> {
    let presence = leave_presence(room, nick)?;
    send_stanza_until(client, presence.into(), deadline)
        .await
        .map_err(|e| format!("failed to leave xmpp muc room: {e}"))
}

fn leave_presence(room: &BareJid, nick: &str) -> Result<Presence, String> {
    let to = muc_occupant_jid(room, nick)?;
    Ok(Presence::unavailable().with_to(to))
}

fn muc_occupant_jid(room: &BareJid, nick: &str) -> Result<Jid, String> {
    Jid::new(&format!("{room}/{nick}")).map_err(|e| format!("invalid muc occupant jid: {e}"))
}

fn muc_presence_from(presence: &Presence, occupant: &Jid) -> bool {
    presence.from.as_ref() == Some(occupant)
}

async fn submit_instant_room_config(client: &mut Client, room: &BareJid) -> Result<(), String> {
    // XEP-0045 instant-room setup: an empty owner data-form submit unlocks a
    // newly-created room using server defaults. This is intentionally not a
    // full privacy or member-affiliation configuration flow.
    let query = instant_room_config_query();
    let token = client
        .send_iq(Some(Jid::from(room.clone())), IqRequest::Set(query))
        .await;
    match tokio::time::timeout(STANZA_TIMEOUT, token).await {
        Ok(Ok(IqResponse::Result(_))) => Ok(()),
        Ok(Ok(IqResponse::Error(error))) => {
            tracing::warn!(target: LOG_TARGET, room = %room, error = ?error, "xmpp muc instant-room owner config rejected");
            Err(format!(
                "xmpp MUC instant-room owner config rejected by server: {:?} {:?}",
                error.type_, error.defined_condition
            ))
        }
        Ok(Err(error)) => {
            tracing::warn!(target: LOG_TARGET, room = %room, %error, "failed to send xmpp muc instant-room owner config iq");
            Err(format!(
                "failed to send xmpp MUC instant-room owner config iq: {error}"
            ))
        }
        Err(_) => Err(format!(
            "timed out waiting for xmpp MUC instant-room owner config result from {room}"
        )),
    }
}

fn instant_room_config_query() -> xmpp_parsers::minidom::Element {
    let form = xmpp_parsers::minidom::Element::builder("x", ns::DATA_FORMS)
        .attr(
            xmpp_parsers::minidom::rxml::xml_ncname!("type").into(),
            "submit",
        )
        .build();
    xmpp_parsers::minidom::Element::builder("query", MUC_OWNER_NS)
        .append(form)
        .build()
}

async fn send_presence(client: &mut Client, presence: Presence) -> Result<(), String> {
    send_stanza_with_timeout(client, presence.into()).await
}

async fn send_stanza_with_timeout(client: &mut Client, stanza: Stanza) -> Result<(), String> {
    tokio::time::timeout(STANZA_TIMEOUT, client.send_stanza(stanza))
        .await
        .map_err(|_| "timed out sending xmpp stanza".to_owned())?
        .map(|_| ())
        .map_err(|e| e.to_string())
}

async fn send_stanza_until(
    client: &mut Client,
    stanza: Stanza,
    deadline: tokio::time::Instant,
) -> Result<(), String> {
    tokio::time::timeout_at(deadline, client.send_stanza(stanza))
        .await
        .map_err(|_| "timed out sending xmpp stanza before shutdown deadline".to_owned())?
        .map(|_| ())
        .map_err(|e| e.to_string())
}

async fn send_chat(client: &mut Client, to: Jid, text: &str) -> Result<(), String> {
    let message = Message::chat(to).with_body(Lang::new(), text.to_owned());
    send_stanza_with_timeout(client, message.into())
        .await
        .map_err(|e| format!("failed to send xmpp chat message: {e}"))
}

async fn send_groupchat(client: &mut Client, to: Jid, text: &str) -> Result<(), String> {
    let message = Message::groupchat(to).with_body(Lang::new(), text.to_owned());
    send_stanza_with_timeout(client, message.into())
        .await
        .map_err(|e| format!("failed to send xmpp groupchat message: {e}"))
}

async fn send_muc_invite(
    client: &mut Client,
    room: BareJid,
    to: Jid,
    reason: &str,
) -> Result<(), String> {
    let message = muc_invite_message(room, to, reason);
    send_stanza_with_timeout(client, message.into())
        .await
        .map_err(|e| format!("failed to send xmpp muc invite: {e}"))
}

fn muc_invite_message(room: BareJid, to: Jid, reason: &str) -> Message {
    let invite = MucUser {
        invite: Some(Invite {
            from: None,
            to: Some(to),
            reason: Some(reason.to_owned()),
        }),
        ..MucUser::new()
    };
    Message::normal(Jid::from(room)).with_payload(invite)
}

fn run_with_bridge<R, W>(
    reader: R,
    writer: W,
    bridge: Arc<dyn XmppBridge>,
) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write + Send + 'static,
{
    let state = tau_client::TauExtensionRunner::new(XmppExtension).run_detached_writer_with_state(
        reader,
        writer,
        move |handle| XmppRuntime {
            ext: Extension::new(bridge, handle),
        },
    )?;
    state.ext.shutdown.request();
    Ok(())
}

struct XmppExtension;

impl TauExtension for XmppExtension {
    type State = XmppRuntime;

    fn name(&self) -> &'static str {
        "tau-ext-xmpp"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder
            .configure_raw(handle_configure)
            .tool_with_group_and_prompt_fragment(
                register_tool_spec(),
                Some(xmpp_tool_group()),
                None,
                handle_tool_invocation,
            )
            .tool_with_group_and_prompt_fragment(
                send_tool_spec(),
                Some(xmpp_tool_group()),
                None,
                handle_tool_invocation,
            )
            .on_raw_restore(
                tau_proto::EventSelector::Exact(tau_proto::EventName::SESSION_STARTED),
                handle_session_started,
            )
            .on_raw_live(
                tau_proto::EventSelector::Exact(tau_proto::EventName::SESSION_STARTED),
                handle_session_started,
            )
            .on_raw_live(
                tau_proto::EventSelector::Exact(tau_proto::EventName::SESSION_AGENT_UNLOADED),
                handle_live_event,
            )
            .on_raw_live(
                tau_proto::EventSelector::Exact(tau_proto::EventName::SESSION_SHUTDOWN),
                handle_live_event,
            )
            .ready_message("xmpp ready");
    }
}

struct XmppRuntime {
    /// Shared XMPP bridge state and background-worker coordination.
    ext: Extension,
}

fn handle_configure(cx: tau_client::RawConfigureContext<'_, XmppRuntime>) -> ClientResult<()> {
    if cx.state.ext.config_is_locked() {
        return Err(ClientError::handler(immutable_config_error()));
    }
    let cfg = match cx.parse_config::<ExtConfig>() {
        Ok(cfg) => cfg,
        Err(error) => {
            cx.state.ext.clear_config_before_start();
            return Err(error);
        }
    };
    let instance_name = cx.instance_name().map(ToString::to_string);
    let cfg = match cfg.validate(cx.secrets(), instance_name) {
        Ok(cfg) => cfg,
        Err(message) => {
            cx.state.ext.clear_config_before_start();
            return Err(ClientError::handler(message));
        }
    };
    if let Err(message) = cx.state.ext.apply_config(cfg) {
        cx.state.ext.clear_config_before_start();
        return Err(ClientError::handler(message));
    }
    Ok(())
}

fn handle_tool_invocation(cx: tau_client::ToolContext<'_, XmppRuntime>) -> ClientResult<()> {
    cx.state.ext.dispatch_tool(cx.invoke().clone());
    Ok(())
}

fn handle_session_started(cx: tau_client::RawEventContext<'_, XmppRuntime>) -> ClientResult<()> {
    if let Event::SessionStarted(started) = cx.event() {
        let mut state = cx.state.ext.state.lock().unwrap_or_else(|e| e.into_inner());
        state.current_session_id = Some(started.session_id.clone());
    }
    Ok(())
}

fn handle_live_event(cx: tau_client::RawEventContext<'_, XmppRuntime>) -> ClientResult<()> {
    match cx.event() {
        Event::SessionAgentUnloaded(unloaded) => {
            unload_agent(&cx.state.ext, unloaded.agent_id.clone());
        }
        Event::SessionShutdown(shutdown) => {
            shutdown_session(&cx.state.ext, shutdown.session_id.clone());
        }
        _ => {}
    }
    Ok(())
}

fn immutable_config_error() -> String {
    "xmpp configuration cannot be changed after the bridge has started; restart Tau to apply new XMPP settings"
        .to_owned()
}

fn unload_agent(ext: &Extension, agent_id: AgentId) {
    let _ = ext.bridge.unregister_agent(&agent_id);
    let mut state = ext.state.lock().unwrap_or_else(|e| e.into_inner());
    state.registered_agents.remove(&agent_id);
    state.conversations.remove(&agent_id);
}

fn shutdown_session(ext: &Extension, session_id: SessionId) {
    let agents: Vec<_> = {
        let mut state = ext.state.lock().unwrap_or_else(|e| e.into_inner());
        let agents = state.registered_agents.iter().cloned().collect::<Vec<_>>();
        state.registered_agents.clear();
        state.conversations.clear();
        if state.current_session_id.as_ref() == Some(&session_id) {
            state.current_session_id = None;
        }
        agents
    };
    for agent in agents {
        let _ = ext.bridge.unregister_agent(&agent);
    }
}

fn xmpp_tool_group() -> tau_proto::ToolGroup {
    tau_proto::ToolGroup {
        name: tau_proto::ToolGroupName::new(TOOL_GROUP_NAME),
        prompt_fragment: None,
    }
}

fn example_field(name: &str, value: CborValue) -> (CborValue, CborValue) {
    (CborValue::Text(name.to_owned()), value)
}

fn example_text(value: &str) -> CborValue {
    CborValue::Text(value.to_owned())
}

fn register_tool_spec() -> ToolSpec {
    ToolSpec {
        name: tau_proto::ToolName::new(REGISTER_TOOL_NAME),
        model_visible_name: Some(tau_proto::ToolName::new(REGISTER_TOOL_NAME)),
        description: Some("Register or unregister the current agent for XMPP messages. Incoming prompts are accepted only from configured allowed_jids. Use xmpp_send to reply to XMPP-originated prompts.".to_owned()),
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "enabled": { "type": "boolean" } },
            "required": ["enabled"]
        })),
        format: None,
        tags: vec![tau_proto::ToolTag::new(REGISTER_TOOL_TAG)],
        enabled_by_default: false,
        background_support: None,
        examples: vec![ToolExample {
            id: "enable-registration".to_owned(),
            title: Some("Register for XMPP".to_owned()),
            arguments: CborValue::Map(vec![example_field("enabled", CborValue::Bool(true))]),
            note: Some("Use enabled=false to stop receiving XMPP prompts.".to_owned()),
            subcommand: None,
        }],
    }
}

fn send_tool_spec() -> ToolSpec {
    ToolSpec {
        name: tau_proto::ToolName::new(SEND_TOOL_NAME),
        model_visible_name: Some(tau_proto::ToolName::new(SEND_TOOL_NAME)),
        description: Some("Send a text reply to this agent's registered XMPP room or direct conversation. There is no destination argument; use xmpp_register first. Replies to room-message prompts are visible to room occupants.".to_owned()),
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "message": { "type": "string" } },
            "required": ["message"]
        })),
        format: None,
        tags: vec![tau_proto::ToolTag::new(SEND_TOOL_TAG)],
        enabled_by_default: false,
        background_support: None,
        examples: vec![ToolExample {
            id: "send-reply".to_owned(),
            title: Some("Send an XMPP reply".to_owned()),
            arguments: CborValue::Map(vec![example_field(
                "message",
                example_text("Thanks, I’ll look into it."),
            )]),
            note: Some("There is no destination argument; the registered conversation is used.".to_owned()),
            subcommand: None,
        }],
    }
}

fn cbor_bool_field(value: &CborValue, name: &str) -> Result<bool, String> {
    match cbor_field(value, name) {
        Some(CborValue::Bool(value)) => Ok(*value),
        Some(_) => Err(format!("`{name}` must be a boolean")),
        None => Err(format!("missing `{name}`")),
    }
}

fn cbor_string_field(value: &CborValue, name: &str) -> Result<String, String> {
    match cbor_field(value, name) {
        Some(CborValue::Text(value)) => Ok(value.clone()),
        Some(_) => Err(format!("`{name}` must be a string")),
        None => Err(format!("missing `{name}`")),
    }
}

fn cbor_field<'a>(value: &'a CborValue, name: &str) -> Option<&'a CborValue> {
    let CborValue::Map(entries) = value else {
        return None;
    };
    entries.iter().find_map(|(key, value)| match key {
        CborValue::Text(key) if key == name => Some(value),
        _ => None,
    })
}

fn cbor_reject_unknown_fields(value: &CborValue, allowed: &[&str]) -> Result<(), String> {
    let CborValue::Map(entries) = value else {
        return Err("tool arguments must be an object".to_owned());
    };
    for (key, _) in entries {
        let CborValue::Text(key) = key else {
            return Err("tool argument names must be strings".to_owned());
        };
        if !allowed.contains(&key.as_str()) {
            return Err(format!("unknown `{key}` argument"));
        }
    }
    Ok(())
}

fn tool_result(invoke: ToolStarted, text: &str) -> Event {
    Event::ToolResult(ToolResult {
        call_id: invoke.call_id,
        tool_name: invoke.tool_name,
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text(text.to_owned()),
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
        call_id: invoke.call_id,
        tool_name: invoke.tool_name,
        tool_type: tau_proto::ToolType::Function,
        message: message.clone(),
        details: Some(invoke.arguments),
        display: Some(ToolUseState {
            status: ToolUseStatus::Error,
            status_text: message,
            ..Default::default()
        }),
        originator: invoke.originator,
    })
}

fn clean_token_or(input: &str, fallback: &str) -> String {
    let mut out: String = input
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        .take(48)
        .collect();
    if out.is_empty() {
        out = fallback.to_owned();
    }
    out
}

fn short_random_hex() -> String {
    let mut bytes = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn generated_resource(cfg: &RuntimeConfig) -> String {
    let instance = cfg
        .instance_name
        .as_deref()
        .map(|name| clean_token_or(name, "session"))
        .unwrap_or_else(|| "session".to_owned());
    format!(
        "{}-{}-{}-{}",
        cfg.resource_prefix,
        instance,
        std::process::id(),
        short_random_hex()
    )
}

/// Return a short, readable, normalization-safe MUC room identity label.
fn muc_room_label(session_id: &SessionId, agent_id: &AgentId) -> String {
    let session_slug = localpart_slug(session_id.as_ref(), MUC_SESSION_SLUG_MAX_CHARS);
    let agent_slug = agent_room_slug(agent_id);
    let disambiguator = muc_room_disambiguator(session_id, agent_id);
    format!("{session_slug}-{agent_slug}-{disambiguator}")
}

fn muc_room_disambiguator(session_id: &SessionId, agent_id: &AgentId) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"tau-ext-xmpp muc room v1\0session\0");
    hasher.update(session_id.as_ref().as_bytes());
    hasher.update(b"\0agent\0");
    hasher.update(agent_id.as_ref().as_bytes());
    let hash = hasher.finalize();
    base32_token(&hash.as_bytes()[..MUC_ROOM_DISAMBIGUATOR_BYTES])
}

fn agent_room_slug(agent_id: &AgentId) -> String {
    let mut segments = localpart_segments(agent_id.as_ref());
    if let Some(suffix) = likely_generated_agent_suffix(agent_id.as_ref())
        && segments.last() == Some(&suffix)
    {
        segments.pop();
    }
    join_slug_segments(&segments, MUC_AGENT_SLUG_MAX_CHARS)
}

fn likely_generated_agent_suffix(input: &str) -> Option<String> {
    // Tau-generated agent ids commonly end with a short mixed-case/digit suffix
    // (for example `manager-Y3KG`). Hide that visual noise from room slugs while
    // still feeding the complete AgentId into the disambiguator.
    input
        .rsplit_once(|ch: char| !ch.is_ascii_alphanumeric())
        .map(|(_, suffix)| suffix)
        .filter(|suffix| {
            (4..=8).contains(&suffix.len())
                && suffix.chars().any(|ch| ch.is_ascii_uppercase())
                && suffix.chars().any(|ch| ch.is_ascii_digit())
        })
        .map(|suffix| suffix.to_ascii_lowercase())
}

fn localpart_slug(input: &str, max_chars: usize) -> String {
    join_slug_segments(&localpart_segments(input), max_chars)
}

fn localpart_segments(input: &str) -> Vec<String> {
    input
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|segment| !segment.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn join_slug_segments(segments: &[String], max_chars: usize) -> String {
    let mut out = String::new();
    for segment in segments {
        if out.len() >= max_chars {
            break;
        }
        if !out.is_empty() {
            out.push('-');
        }
        let remaining = max_chars.saturating_sub(out.len());
        out.extend(segment.chars().take(remaining));
        while out.ends_with('-') {
            out.pop();
        }
    }
    if out.is_empty() { "x".to_owned() } else { out }
}

fn base32_token(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";
    let mut out = String::new();
    let mut buffer = 0u16;
    let mut bits = 0u8;
    for byte in bytes {
        buffer = (buffer << 8) | u16::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            let index = usize::from((buffer >> bits) & 0b1_1111);
            out.push(char::from(ALPHABET[index]));
        }
    }
    if bits > 0 {
        let index = usize::from((buffer << (5 - bits)) & 0b1_1111);
        out.push(char::from(ALPHABET[index]));
    }
    out
}

fn has_delay_payload(message: &Message) -> bool {
    message
        .payloads
        .iter()
        .any(|payload| Delay::try_from(payload.clone()).is_ok())
}

#[cfg(test)]
mod tests;
