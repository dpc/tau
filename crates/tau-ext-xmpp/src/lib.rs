//! Personal XMPP bridge extension for Tau agents.
//!
//! The extension exposes `xmpp_register` and `xmpp_send`. It is disabled by
//! default, uses a mandatory JID allowlist, and treats XMPP text as external
//! untrusted prompt input.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error;
use std::io::{BufReader, BufWriter, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use futures_util::StreamExt;
use rand::RngCore;
use tau_proto::{
    AgentId, CborValue, ConfigError, Event, ExtPromptSubmitRequest, HarnessInputMessage,
    HarnessOutputMessage, PeerInputReader, PeerOutputWriter, ToolError, ToolProgress, ToolResult,
    ToolSpec, ToolStarted, ToolUseState, ToolUseStatus,
};
use tokio_xmpp::Client;
use xmpp_parsers::jid::{BareJid, Jid};
use xmpp_parsers::message::{Lang, Message, MessageType};
use xmpp_parsers::muc::muc::History;
use xmpp_parsers::muc::{Muc, MucUser};
use xmpp_parsers::presence::{Presence, Type as PresenceType};
use xmpp_parsers::stanza::Stanza;

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
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const REGISTER_TIMEOUT: Duration = Duration::from_secs(45);
const STANZA_TIMEOUT: Duration = Duration::from_secs(20);

/// Run the XMPP extension over stdio.
pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    tau_extension::init_logging_for(LOG_TARGET);
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

/// Small bridge surface used by the extension and faked by unit tests.
trait XmppBridge: Send + Sync + 'static {
    /// Ensure the underlying XMPP task is started.
    fn ensure_started(
        &self,
        cfg: RuntimeConfig,
        tx: mpsc::Sender<HarnessInputMessage>,
        shutdown: Arc<AtomicBool>,
    ) -> Result<(), String>;

    /// Register one agent conversation and return its XMPP address.
    fn register_agent(&self, cfg: &RuntimeConfig, agent_id: &AgentId) -> Result<String, String>;

    /// Remove one registered agent conversation from the bridge.
    fn unregister_agent(&self, agent_id: &AgentId) -> Result<(), String>;

    /// Send text to the registered agent's conversation.
    fn send_message(&self, agent_id: &AgentId, text: &str) -> Result<(), String>;
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
        let account_text = self
            .jid
            .ok_or_else(|| "xmpp config requires `jid`".to_owned())?;
        let account = Jid::new(&account_text).map_err(|e| format!("invalid xmpp `jid`: {e}"))?;
        if account.resource().is_some() {
            return Err(
                "xmpp `jid` must be a bare account JID; Tau generates unique resources".to_owned(),
            );
        }
        let secret_name = self
            .password_secret
            .ok_or_else(|| "xmpp config requires `password_secret`".to_owned())?;
        let password = secrets
            .get(&secret_name)
            .map(tau_proto::SecretValue::expose_secret)
            .filter(|password| !password.trim().is_empty())
            .ok_or_else(|| format!("xmpp secret `{secret_name}` is missing or empty"))?;
        if self.allowed_jids.is_empty() {
            return Err("xmpp config requires non-empty `allowed_jids`".to_owned());
        }
        let allowed_jids = self
            .allowed_jids
            .iter()
            .map(|entry| AllowedJid::parse(entry))
            .collect::<Result<Vec<_>, _>>()?;
        let default_text = self
            .default_recipient
            .ok_or_else(|| "xmpp config requires `default_recipient`".to_owned())?;
        let default_recipient = Jid::new(&default_text)
            .map_err(|e| format!("invalid xmpp `default_recipient`: {e}"))?;
        if !allowed_jids
            .iter()
            .any(|allowed| allowed.matches(&default_recipient))
        {
            return Err("xmpp `default_recipient` must match `allowed_jids`".to_owned());
        }
        let routing_mode = match self.routing.mode.as_deref().unwrap_or("muc") {
            "muc" => RoutingMode::Muc,
            "direct_resource" => RoutingMode::DirectResource,
            other => return Err(format!("unsupported xmpp routing.mode `{other}`")),
        };
        let muc_service = match self.muc.service {
            Some(service) => Some(
                Jid::new(&service)
                    .map_err(|e| format!("invalid xmpp muc.service: {e}"))?
                    .to_bare(),
            ),
            None => None,
        };
        if routing_mode == RoutingMode::Muc && muc_service.is_none() {
            return Err("xmpp routing.mode `muc` requires `muc.service`".to_owned());
        }
        let max_message_bytes = self.max_message_bytes.unwrap_or(DEFAULT_MESSAGE_LIMIT);
        if max_message_bytes == 0 {
            return Err("xmpp `max_message_bytes` must be greater than zero".to_owned());
        }
        Ok(RuntimeConfig {
            account_jid: account.to_bare(),
            password: password.to_owned(),
            allowed_jids,
            default_recipient,
            routing_mode,
            resource_prefix: clean_token(
                self.resource_prefix
                    .as_deref()
                    .unwrap_or(DEFAULT_RESOURCE_PREFIX),
            ),
            muc: MucConfig {
                service: muc_service,
                room_prefix: clean_token(
                    self.muc
                        .room_prefix
                        .as_deref()
                        .unwrap_or(DEFAULT_ROOM_PREFIX),
                ),
                expose_real_jids: self.muc.expose_real_jids.unwrap_or(true),
                trust_muc_membership: self.muc.trust_muc_membership.unwrap_or(false),
                invite_default_recipient: self.muc.invite_default_recipient.unwrap_or(true),
            },
            max_message_bytes,
            instance_name,
        })
    }
}

#[derive(Default)]
struct State {
    /// Validated runtime config.
    config: Option<RuntimeConfig>,
    /// Agents currently registered with the bridge.
    registered_agents: HashSet<AgentId>,
    /// Human-readable agent labels.
    agent_labels: HashMap<AgentId, String>,
    /// XMPP conversation address per agent.
    conversations: HashMap<AgentId, String>,
    /// Whether the XMPP bridge has been started.
    bridge_started: bool,
}

struct Extension {
    /// Shared runtime state.
    state: Arc<Mutex<State>>,
    /// XMPP bridge implementation.
    bridge: Arc<dyn XmppBridge>,
    /// Writer channel toward the harness.
    tx: mpsc::Sender<HarnessInputMessage>,
    /// Shared shutdown flag.
    shutdown: Arc<AtomicBool>,
}

impl Extension {
    fn new(bridge: Arc<dyn XmppBridge>, tx: mpsc::Sender<HarnessInputMessage>) -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            bridge,
            tx,
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    fn apply_config(&self, cfg: RuntimeConfig) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.config = Some(cfg);
    }

    fn dispatch_tool(&self, invoke: ToolStarted) {
        let _ = self.tx.send(HarnessInputMessage::emit(Event::ToolProgress(
            ToolProgress {
                call_id: invoke.call_id.clone(),
                tool_name: invoke.tool_name.clone(),
                message: Some("xmpp tool started".to_owned()),
                progress: None,
                display: Some(ToolUseState {
                    status: ToolUseStatus::InProgress,
                    status_text: tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
                    ..Default::default()
                }),
            },
        )));
        let event = match invoke.tool_name.as_str() {
            REGISTER_TOOL_NAME => self.handle_register(invoke),
            SEND_TOOL_NAME => self.handle_send(invoke),
            _ => tool_error(invoke, "unknown xmpp tool".to_owned()),
        };
        let _ = self.tx.send(HarnessInputMessage::emit(event));
    }

    fn handle_register(&self, invoke: ToolStarted) -> Event {
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
                if !state.bridge_started {
                    if let Err(message) = self.bridge.ensure_started(
                        cfg.clone(),
                        self.tx.clone(),
                        Arc::clone(&self.shutdown),
                    ) {
                        return tool_error(invoke, message);
                    }
                    state.bridge_started = true;
                }
                cfg
            };
            let address = match self.bridge.register_agent(&cfg, &invoke.agent_id) {
                Ok(address) => address,
                Err(message) => return tool_error(invoke, message),
            };
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.registered_agents.insert(invoke.agent_id.clone());
            state
                .agent_labels
                .entry(invoke.agent_id.clone())
                .or_insert_with(|| invoke.agent_id.to_string());
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
        let message = match cbor_string_field(&invoke.arguments, "message") {
            Ok(message) => message,
            Err(message) => return tool_error(invoke, message),
        };
        if message.trim().is_empty() {
            return tool_error(invoke, "`message` must not be empty".to_owned());
        }
        {
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
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

/// Live tokio-xmpp bridge.
#[derive(Default)]
struct LiveXmppBridge {
    /// Command channel to the XMPP worker.
    command_tx: Mutex<Option<mpsc::Sender<XmppCommand>>>,
}

enum XmppCommand {
    Register {
        /// Agent to register.
        agent_id: AgentId,
        /// Response channel carrying the conversation address.
        response: mpsc::Sender<Result<String, String>>,
    },
    Unregister {
        /// Agent to unregister.
        agent_id: AgentId,
    },
    Send {
        /// Sending agent.
        agent_id: AgentId,
        /// Text body to send.
        text: String,
        /// Response channel.
        response: mpsc::Sender<Result<(), String>>,
    },
}

impl XmppBridge for LiveXmppBridge {
    fn ensure_started(
        &self,
        cfg: RuntimeConfig,
        tx: mpsc::Sender<HarnessInputMessage>,
        shutdown: Arc<AtomicBool>,
    ) -> Result<(), String> {
        let mut guard = self.command_tx.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_some() {
            return Ok(());
        }
        let (command_tx, command_rx) = mpsc::channel();
        let worker_tx = command_tx.clone();
        std::thread::Builder::new()
            .name("tau-ext-xmpp".to_owned())
            .spawn(move || xmpp_thread(cfg, command_rx, tx, shutdown))
            .map_err(|e| format!("failed to spawn xmpp worker: {e}"))?;
        *guard = Some(worker_tx);
        Ok(())
    }

    fn register_agent(&self, _cfg: &RuntimeConfig, agent_id: &AgentId) -> Result<String, String> {
        let tx = self.command_sender()?;
        let (response_tx, response_rx) = mpsc::channel();
        tx.send(XmppCommand::Register {
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
        tx.send(XmppCommand::Unregister {
            agent_id: agent_id.clone(),
        })
        .map_err(|_| "xmpp worker is not running".to_owned())
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
    tx: mpsc::Sender<HarnessInputMessage>,
    shutdown: Arc<AtomicBool>,
) {
    match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(xmpp_worker(cfg, command_rx, tx, shutdown)),
        Err(error) => tracing::warn!(target: LOG_TARGET, %error, "failed to create xmpp runtime"),
    }
}

async fn xmpp_worker(
    cfg: RuntimeConfig,
    command_rx: mpsc::Receiver<XmppCommand>,
    tx: mpsc::Sender<HarnessInputMessage>,
    shutdown: Arc<AtomicBool>,
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
    let mut worker = WorkerState::new(cfg, tx);
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        tokio::select! {
            event = client.next() => {
                let Some(event) = event else { return; };
                match event {
                    tokio_xmpp::Event::Online { bound_jid, .. } => {
                        worker.bound_jid = Some(bound_jid.clone());
                        worker.occupant_real_jids.clear();
                        let _ = send_presence(&mut client, Presence::available().with_priority(-1)).await;
                        worker.rejoin_all(&mut client).await;
                    }
                    tokio_xmpp::Event::Disconnected(error) => {
                        tracing::warn!(target: LOG_TARGET, %error, "xmpp disconnected");
                    }
                    tokio_xmpp::Event::Stanza(stanza) => worker.handle_stanza(stanza),
                }
            }
            command = command_rx.recv() => {
                let Some(command) = command else { return; };
                worker.handle_command(command, &mut client).await;
            }
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
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

struct WorkerState {
    /// Runtime config.
    cfg: RuntimeConfig,
    /// Writer channel toward the harness.
    tx: mpsc::Sender<HarnessInputMessage>,
    /// Server-returned bound JID.
    bound_jid: Option<Jid>,
    /// Registered conversations.
    conversations: HashMap<AgentId, Conversation>,
    /// MUC room to agent mapping.
    room_to_agent: HashMap<BareJid, AgentId>,
    /// MUC occupant real JID cache.
    occupant_real_jids: HashMap<Jid, Jid>,
    /// Per-worker random token used to make room names session-specific.
    session_token: String,
}

impl WorkerState {
    /// Create a worker state.
    fn new(cfg: RuntimeConfig, tx: mpsc::Sender<HarnessInputMessage>) -> Self {
        Self {
            cfg,
            tx,
            bound_jid: None,
            conversations: HashMap::new(),
            room_to_agent: HashMap::new(),
            occupant_real_jids: HashMap::new(),
            session_token: short_random_hex(),
        }
    }

    /// Process one command from tool handlers.
    async fn handle_command(&mut self, command: XmppCommand, client: &mut Client) {
        match command {
            XmppCommand::Register { agent_id, response } => {
                let result = match tokio::time::timeout(
                    REGISTER_TIMEOUT,
                    self.register_agent(agent_id.clone(), client),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => Err("timed out registering xmpp conversation".to_owned()),
                };
                self.finish_register_response(&agent_id, result, response);
            }
            XmppCommand::Unregister { agent_id } => {
                self.conversations.remove(&agent_id);
                self.room_to_agent.retain(|_, mapped| mapped != &agent_id);
            }
            XmppCommand::Send {
                agent_id,
                text,
                response,
            } => {
                let result = self.send_message(&agent_id, &text, client).await;
                let _ = response.send(result);
            }
        }
    }

    /// Send a register response and roll back worker routing if the caller has
    /// already timed out and dropped its receiver.
    fn finish_register_response(
        &mut self,
        agent_id: &AgentId,
        result: Result<String, String>,
        response: mpsc::Sender<Result<String, String>>,
    ) {
        let registered = result.is_ok();
        if response.send(result).is_err() && registered {
            self.conversations.remove(agent_id);
            self.room_to_agent.retain(|_, mapped| mapped != agent_id);
        }
    }

    /// Register one agent conversation.
    async fn register_agent(
        &mut self,
        agent_id: AgentId,
        client: &mut Client,
    ) -> Result<String, String> {
        if let Some(conversation) = self.conversations.get(&agent_id) {
            return Ok(conversation.address());
        }
        let conversation = match self.cfg.routing_mode {
            RoutingMode::Muc => {
                let service = self
                    .cfg
                    .muc
                    .service
                    .clone()
                    .ok_or_else(|| "xmpp muc.service is not configured".to_owned())?;
                let room = Jid::new(&format!(
                    "{}-{}-{}@{}",
                    self.cfg.muc.room_prefix,
                    clean_token(agent_id.as_ref()),
                    self.session_token,
                    service.domain()
                ))
                .map_err(|e| format!("failed to build muc room jid: {e}"))?
                .to_bare();
                let nick = format!("{}-{}", self.cfg.resource_prefix, short_random_hex());
                join_room(client, &room, &nick).await?;
                if self.cfg.muc.invite_default_recipient {
                    let notice = format!(
                        "Tau agent {} registered an XMPP room: {} (plaintext over TLS; no OMEMO/E2EE).",
                        agent_id.as_ref(),
                        room
                    );
                    send_chat(client, self.cfg.default_recipient.clone(), &notice).await?;
                }
                self.room_to_agent.insert(room.clone(), agent_id.clone());
                Conversation::Muc { room, nick }
            }
            RoutingMode::DirectResource => {
                if self
                    .conversations
                    .values()
                    .any(|conversation| matches!(conversation, Conversation::Direct { .. }))
                {
                    return Err("direct_resource mode supports only one registered agent per extension instance".to_owned());
                }
                let Some(bound) = self.bound_jid.clone() else {
                    return Err("xmpp connection is not online yet".to_owned());
                };
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
        self.conversations.insert(agent_id, conversation);
        Ok(address)
    }

    /// Send one message to a registered conversation.
    async fn send_message(
        &self,
        agent_id: &AgentId,
        text: &str,
        client: &mut Client,
    ) -> Result<(), String> {
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

    /// Rejoin all known MUC rooms after an online/reconnect event.
    async fn rejoin_all(&self, client: &mut Client) {
        for conversation in self.conversations.values() {
            if let Conversation::Muc { room, nick } = conversation {
                let _ = join_room(client, room, nick).await;
            }
        }
    }

    /// Process an inbound stanza.
    fn handle_stanza(&mut self, stanza: Stanza) {
        match stanza {
            Stanza::Message(message) => self.handle_message(message),
            Stanza::Presence(presence) => self.handle_presence(presence),
            Stanza::Iq(_) => {}
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
            MessageType::Error | MessageType::Headline => {}
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
            return;
        }
        let source = real.map_or_else(|| from.to_string(), |jid| jid.to_string());
        self.route(agent_id, format!("[xmpp room {room} from {source}] {body}"));
    }

    /// Process inbound direct chat fallback.
    fn handle_direct(&mut self, message: Message, body: String) {
        let Some(from) = message.from.clone() else {
            return;
        };
        if !self.cfg.is_allowed(&from) {
            return;
        }
        let Some(to) = message.to.as_ref() else {
            return;
        };
        let Some(bound) = self.bound_jid.as_ref() else {
            return;
        };
        if to != bound {
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
            return;
        }
        self.route(
            agents[0].clone(),
            format!("[xmpp direct from {from}] {body}"),
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
        let _ = self
            .tx
            .send(HarnessInputMessage::emit(Event::ExtPromptSubmitRequest(
                ExtPromptSubmitRequest {
                    agent_id,
                    text,
                    ctx_id: None,
                },
            )));
    }
}

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

impl Conversation {
    /// Return the user-visible conversation address.
    fn address(&self) -> String {
        match self {
            Self::Muc { room, .. } => room.to_string(),
            Self::Direct { full_jid } => full_jid.to_string(),
        }
    }
}

async fn join_room(client: &mut Client, room: &BareJid, nick: &str) -> Result<(), String> {
    let to = Jid::new(&format!("{room}/{nick}"))
        .map_err(|e| format!("invalid muc occupant jid: {e}"))?;
    let presence = Presence::available()
        .with_to(to)
        .with_payload(Muc::new().with_history(History::new().with_maxstanzas(0)));
    send_presence(client, presence)
        .await
        .map_err(|e| format!("failed to join xmpp muc room: {e}"))
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

fn run_with_bridge<R, W>(
    reader: R,
    writer: W,
    bridge: Arc<dyn XmppBridge>,
) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write + Send + 'static,
{
    let mut reader = PeerInputReader::new(BufReader::new(reader));
    let mut writer = PeerOutputWriter::new(BufWriter::new(writer));
    tau_extension::Handshake::tool("tau-ext-xmpp")
        .subscribe([
            tau_proto::EventName::TOOL_STARTED,
            tau_proto::EventName::AGENT_DISPLAY_NAME_SET,
            tau_proto::EventName::AGENT_STARTED,
            tau_proto::EventName::SESSION_AGENT_UNLOADED,
            tau_proto::EventName::SESSION_SHUTDOWN,
        ])
        .register_tool_with_group_and_prompt_fragment(
            register_tool_spec(),
            Some(xmpp_tool_group()),
            None,
        )
        .register_tool_with_group_and_prompt_fragment(
            send_tool_spec(),
            Some(xmpp_tool_group()),
            None,
        )
        .ready_message("xmpp ready")
        .run(&mut writer)?;

    let (tx, rx) = mpsc::channel::<HarnessInputMessage>();
    let ext = Extension::new(bridge, tx.clone());
    let writer_handle = std::thread::spawn(move || -> Result<(), Box<dyn Error + Send>> {
        for msg in rx {
            writer
                .write_message(&msg)
                .map_err(|e| -> Box<dyn Error + Send> { Box::new(e) })?;
            writer
                .flush()
                .map_err(|e| -> Box<dyn Error + Send> { Box::new(e) })?;
        }
        Ok(())
    });

    while let Some(message) = reader.read_message()? {
        match message {
            HarnessOutputMessage::Configure(msg) => {
                match tau_extension::parse_config::<ExtConfig>(&msg.config).and_then(|cfg| {
                    cfg.validate(&msg.secrets, msg.instance_name.map(|name| name.to_string()))
                }) {
                    Ok(cfg) => ext.apply_config(cfg),
                    Err(message) => {
                        let _ = tx.send(HarnessInputMessage::ConfigError(ConfigError { message }));
                    }
                }
            }
            HarnessOutputMessage::Deliver(delivery) => {
                if delivery.is_replay() {
                    continue;
                }
                match delivery.into_event() {
                    Event::ToolStarted(invoke)
                        if matches!(
                            invoke.tool_name.as_str(),
                            REGISTER_TOOL_NAME | SEND_TOOL_NAME
                        ) =>
                    {
                        ext.dispatch_tool(invoke);
                    }
                    Event::AgentDisplayNameSet(name) => {
                        let mut state = ext.state.lock().unwrap_or_else(|e| e.into_inner());
                        state.agent_labels.insert(name.agent_id, name.display_name);
                    }
                    Event::AgentStarted(started) => {
                        if let Some(display_name) = started.display_name {
                            let mut state = ext.state.lock().unwrap_or_else(|e| e.into_inner());
                            state.agent_labels.insert(started.agent_id, display_name);
                        }
                    }
                    Event::SessionAgentUnloaded(unloaded) => {
                        let _ = ext.bridge.unregister_agent(&unloaded.agent_id);
                        let mut state = ext.state.lock().unwrap_or_else(|e| e.into_inner());
                        state.registered_agents.remove(&unloaded.agent_id);
                        state.agent_labels.remove(&unloaded.agent_id);
                        state.conversations.remove(&unloaded.agent_id);
                    }
                    Event::SessionShutdown(_) => {
                        let agents: Vec<_> = {
                            let mut state = ext.state.lock().unwrap_or_else(|e| e.into_inner());
                            let agents =
                                state.registered_agents.iter().cloned().collect::<Vec<_>>();
                            state.registered_agents.clear();
                            state.conversations.clear();
                            agents
                        };
                        for agent in agents {
                            let _ = ext.bridge.unregister_agent(&agent);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    drop(ext);
    drop(tx);
    match writer_handle.join() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(error),
        Err(_) => Err("xmpp writer thread panicked".into()),
    }
}

fn xmpp_tool_group() -> tau_proto::ToolGroup {
    tau_proto::ToolGroup {
        name: tau_proto::ToolGroupName::new(TOOL_GROUP_NAME),
        prompt_fragment: None,
    }
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
    }
}

fn send_tool_spec() -> ToolSpec {
    ToolSpec {
        name: tau_proto::ToolName::new(SEND_TOOL_NAME),
        model_visible_name: Some(tau_proto::ToolName::new(SEND_TOOL_NAME)),
        description: Some("Send a text reply to this agent's registered XMPP conversation. There is no destination argument; use xmpp_register first.".to_owned()),
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

fn clean_token(input: &str) -> String {
    let mut out: String = input
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        .take(48)
        .collect();
    if out.is_empty() {
        out = DEFAULT_RESOURCE_PREFIX.to_owned();
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
        .map(clean_token)
        .unwrap_or_else(|| "session".to_owned());
    format!(
        "{}-{}-{}-{}",
        cfg.resource_prefix,
        instance,
        std::process::id(),
        short_random_hex()
    )
}

#[cfg(test)]
mod tests;
