//! Personal Telegram bridge extension for Tau agents.
//!
//! By default the extension exposes `telegram_register` and `telegram_send`
//! tools. It keeps listener registrations in memory and uses the Telegram Bot
//! API only after an agent registers or another Telegram action needs the
//! client.

mod update_stream_lock;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
use std::time::Duration;

use tau_client::{ClientHandle, ClientResult, ExtensionBuilder, ManualRuntimeInput, TauExtension};
use tau_proto::{
    AgentId, CborValue, Event, ExtPromptSubmitRequest, HarnessInputMessage, HarnessNotice,
    NoticeLevel, ToolError, ToolExample, ToolProgress, ToolResult, ToolSpec, ToolStarted,
    ToolUseState, ToolUseStatus,
};
use update_stream_lock::UpdateStreamLock;

/// Tracing target used by this extension.
pub const LOG_TARGET: &str = "telegram";

/// Legacy `std-telegram` tool name for registering the current agent as a
/// Telegram listener.
pub const REGISTER_TOOL_NAME: &str = "telegram_register";

/// Legacy `std-telegram` tool name for sending a Telegram message from a
/// registered agent.
pub const SEND_TOOL_NAME: &str = "telegram_send";

/// Legacy `std-telegram` tool group name shared by Telegram bridge tools.
pub const TOOL_GROUP_NAME: &str = "telegram";

/// Tag marking tools that register an agent with the Telegram bridge.
pub const REGISTER_TOOL_TAG: &str = "telegram:register";

/// Tag marking tools that send messages through the Telegram bridge.
pub const SEND_TOOL_TAG: &str = "telegram:send";

const LEGACY_INSTANCE_NAME: &str = "std-telegram";
const DEFAULT_API_BASE: &str = "https://api.telegram.org";
const DEFAULT_POLL_TIMEOUT_SECONDS: u64 = 25;
const HTTP_TIMEOUT: Duration = Duration::from_secs(35);
const MAX_DIAGNOSTIC_TEXT_BYTES: usize = 1024;

/// Run the Telegram extension over stdio.
pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    tau_client::init_logging_for(LOG_TARGET);
    run(std::io::stdin(), std::io::stdout())
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
    /// Fetch webhook status without consuming the update stream.
    fn get_webhook_info(&self, cfg: &RuntimeConfig) -> Result<TgWebhookInfo, String>;

    /// Fetch message updates from Telegram using the configured poll timeout.
    fn get_updates(
        &self,
        cfg: &RuntimeConfig,
        offset: Option<i64>,
    ) -> Result<Vec<TgUpdate>, String>;

    /// Send a plain text message to one configured or linked chat.
    fn send_message(&self, cfg: &RuntimeConfig, chat_id: i64, text: &str) -> Result<(), String>;
}

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

impl RuntimeConfig {
    /// Telegram update offsets are scoped to the Bot API endpoint plus bot
    /// token; switching either value starts reading a different update stream.
    fn uses_same_update_stream_as(&self, other: &Self) -> bool {
        self.api_base == other.api_base && self.bot_token == other.bot_token
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
    /// Build the legacy un-namespaced tool set used by `std-telegram`.
    fn legacy() -> Self {
        Self::from_namespace(TOOL_GROUP_NAME).expect("legacy telegram namespace is valid")
    }

    /// Build tool names from the configured namespace.
    fn from_namespace(namespace: &str) -> Result<Self, String> {
        validate_tool_namespace(namespace)?;
        let register = format!("{namespace}_register");
        let send = format!("{namespace}_send");
        let register = tau_proto::ToolName::try_new(&register).ok_or_else(|| {
            format!("telegram `tool_namespace` produces invalid tool name `{register}`")
        })?;
        let send = tau_proto::ToolName::try_new(&send).ok_or_else(|| {
            format!("telegram `tool_namespace` produces invalid tool name `{send}`")
        })?;
        let group = tau_proto::ToolGroupName::try_new(namespace).ok_or_else(|| {
            format!("telegram `tool_namespace` produces invalid tool group `{namespace}`")
        })?;
        Ok(Self {
            namespace: namespace.to_owned(),
            register,
            send,
            group,
        })
    }

    /// Choose explicit config namespace, legacy default, or instance-derived
    /// namespace.
    fn from_config_and_instance(
        cfg: &ExtConfig,
        instance_name: Option<&tau_proto::ExtensionName>,
    ) -> Result<Self, String> {
        if let Some(namespace) = cfg.tool_namespace.as_deref() {
            return Self::from_namespace(namespace);
        }
        match instance_name.map(tau_proto::ExtensionName::as_ref) {
            None | Some(LEGACY_INSTANCE_NAME) => Ok(Self::legacy()),
            Some(name) => Self::from_namespace(&namespace_from_instance_name(name)?),
        }
    }
}

/// Raw deserialized extension config from `harness.yaml`.
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ExtConfig {
    /// Optional model-visible tool namespace/prefix for multi-bot setups.
    tool_namespace: Option<String>,
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
}

impl ExtConfig {
    fn validate(
        self,
        secrets: &BTreeMap<String, tau_proto::SecretValue>,
    ) -> Result<RuntimeConfig, String> {
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
        Ok(RuntimeConfig {
            bot_token: token.to_owned(),
            allowed_user_ids: self.allowed_user_ids.into_iter().collect(),
            configured_chat_id: self.chat_id,
            api_base,
            poll_timeout_seconds,
        })
    }
}

/// A Telegram update containing a message, if present.
#[derive(Clone, Debug, Eq, PartialEq)]
struct TgUpdate {
    /// Telegram update id used for offset advancement.
    update_id: i64,
    /// Text message payload, or `None` for updates kept only to advance offset.
    message: Option<TgMessage>,
}

/// Telegram webhook state relevant to long-poll ownership.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct TgWebhookInfo {
    /// Configured webhook URL, empty when the bot is in getUpdates mode.
    url: String,
    /// Telegram's reported number of pending updates, if present.
    pending_update_count: Option<i64>,
    /// Last webhook delivery error, if Telegram reported one.
    last_error_message: Option<String>,
}

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
    offset: Option<i64>,
    /// Configuration generation captured for stale-response checks.
    config_generation: ConfigGeneration,
    /// Coordination generation observed before this request.
    coordination_generation: u64,
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

#[derive(Default)]
struct State {
    /// Monotonic counter for poller coordination-relevant state changes.
    coordination_generation: u64,
    /// Whether the extension is shutting down and local poller waits should
    /// end.
    shutdown_requested: bool,
    config: Option<RuntimeConfig>,
    /// Harness-provided user-scoped state directory for this extension
    /// instance.
    state_dir: Option<std::path::PathBuf>,
    config_generation: ConfigGeneration,
    registered_agents: HashSet<AgentId>,
    agent_labels: HashMap<AgentId, String>,
    selected_agent_by_chat: HashMap<i64, AgentId>,
    learned_chat: Option<LinkedChat>,
    poller_started: bool,
    /// Held OS advisory lock for the singleton Telegram update stream.
    update_stream_lock: Option<Arc<UpdateStreamLock>>,
    poller_drained_initial_backlog: bool,
    next_update_offset: Option<i64>,
}

/// Shared Telegram state plus a condition variable for waking the poller.
struct SharedState {
    /// Mutable extension state guarded by a single process-local mutex.
    state: Mutex<State>,
    /// Wakes local poller waits after configuration, registration, or shutdown.
    changed: Condvar,
}

impl SharedState {
    /// Create an empty shared state cell.
    fn new() -> Self {
        Self {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
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
}

impl State {
    /// Record a change to config, registration, or shutdown that affects waits.
    fn mark_coordination_changed(&mut self) {
        self.coordination_generation = self.coordination_generation.wrapping_add(1);
    }

    /// Acquire the advisory stream lock unless this state already holds it.
    fn ensure_update_stream_locked(&mut self, cfg: &RuntimeConfig) -> Result<(), String> {
        if self
            .update_stream_lock
            .as_ref()
            .is_some_and(|lock| lock.covers(cfg))
        {
            return Ok(());
        }
        let state_dir = self.state_dir.as_deref().ok_or_else(|| {
            "telegram update polling requires an extension state directory for advisory locking"
                .to_owned()
        })?;
        self.update_stream_lock = Some(Arc::new(UpdateStreamLock::acquire(state_dir, cfg)?));
        Ok(())
    }

    /// Clear active runtime bridge state after config loss or fail-closed
    /// errors.
    fn clear_active_bridge_state(&mut self) {
        self.config = None;
        self.update_stream_lock = None;
        self.registered_agents.clear();
        self.selected_agent_by_chat.clear();
        self.learned_chat = None;
        self.poller_drained_initial_backlog = false;
        self.next_update_offset = None;
    }
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
    /// Telegram poller and tool output is best-effort once the harness has
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

    fn emit(&self, event: Event) {
        self.send(HarnessInputMessage::emit(event));
    }
}

struct Extension {
    state: Arc<SharedState>,
    client: Arc<dyn TelegramClient>,
    output: Output,
    shutdown: Arc<AtomicBool>,
    tool_names: ToolNames,
}

impl Extension {
    fn new(client: Arc<dyn TelegramClient>, output: impl Into<Output>) -> Self {
        Self::with_tool_names(client, output, ToolNames::legacy())
    }

    fn with_tool_names(
        client: Arc<dyn TelegramClient>,
        output: impl Into<Output>,
        tool_names: ToolNames,
    ) -> Self {
        Self {
            state: Arc::new(SharedState::new()),
            client,
            output: output.into(),
            shutdown: Arc::new(AtomicBool::new(false)),
            tool_names,
        }
    }

    fn apply_config(
        &self,
        cfg: RuntimeConfig,
        state_dir: Option<std::path::PathBuf>,
    ) -> Result<(), String> {
        let mut state = self.state.lock();
        state.config_generation = state.config_generation.next();
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
        Ok(())
    }

    fn clear_config_after_error(&self) {
        let mut state = self.state.lock();
        state.config_generation = state.config_generation.next();
        state.clear_active_bridge_state();
        state.mark_coordination_changed();
        self.state.notify_all();
    }

    fn request_shutdown(&self) {
        let mut state = self.state.lock();
        state.shutdown_requested = true;
        state.mark_coordination_changed();
        self.shutdown.store(true, Ordering::Relaxed);
        self.state.notify_all();
    }

    fn poll_response_matches_config(&self, config_generation: ConfigGeneration) -> bool {
        let state = self.state.lock();
        state.config.is_some() && state.config_generation == config_generation
    }

    fn dispatch_tool(&self, invoke: ToolStarted) {
        self.output.emit(Event::ToolProgress(ToolProgress {
            call_id: invoke.call_id.clone(),
            tool_name: invoke.tool_name.clone(),
            message: Some("telegram tool started".to_owned()),
            progress: None,
            display: Some(ToolUseState {
                status: ToolUseStatus::InProgress,
                status_text: tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
                ..Default::default()
            }),
        }));
        let event = match invoke.tool_name.as_str() {
            name if name == self.tool_names.register.as_str() => self.handle_register(invoke),
            name if name == self.tool_names.send.as_str() => self.handle_send(invoke),
            _ => tool_error(invoke, "unknown telegram tool".to_owned()),
        };
        self.output.emit(event);
    }

    fn handles_tool(&self, tool_name: &str) -> bool {
        tool_name == self.tool_names.register.as_str() || tool_name == self.tool_names.send.as_str()
    }

    fn handle_register(&self, invoke: ToolStarted) -> Event {
        if let Err(message) = validate_object_fields(&invoke.arguments, &["enabled"]) {
            return tool_error(invoke, message);
        }
        let enabled = match cbor_bool_field(&invoke.arguments, "enabled") {
            Ok(enabled) => enabled,
            Err(message) => return tool_error(invoke, message),
        };
        let mut state = self.state.lock();
        if enabled {
            let was_unregistered = state.registered_agents.is_empty();
            let cfg = match state.config.clone() {
                Some(cfg) => cfg,
                None => {
                    return tool_error(invoke, "telegram extension is not configured".to_owned());
                }
            };
            if was_unregistered {
                if let Err(message) = state.ensure_update_stream_locked(&cfg) {
                    return tool_error(invoke, message);
                }
                let config_generation = state.config_generation;
                drop(state);
                if let Err(message) = self.check_webhook_allows_get_updates(&cfg, config_generation)
                {
                    return tool_error(invoke, message);
                }
                state = self.state.lock();
                if state.config_generation != config_generation
                    || state
                        .config
                        .as_ref()
                        .is_none_or(|current| !current.uses_same_update_stream_as(&cfg))
                {
                    return tool_error(
                        invoke,
                        "telegram configuration changed while checking webhook status".to_owned(),
                    );
                }
                if !state
                    .update_stream_lock
                    .as_ref()
                    .is_some_and(|lock| lock.covers(&cfg))
                {
                    return tool_error(
                        invoke,
                        "telegram update-stream lock was lost while checking webhook status"
                            .to_owned(),
                    );
                }
            } else if !state
                .update_stream_lock
                .as_ref()
                .is_some_and(|lock| lock.covers(&cfg))
            {
                return tool_error(
                    invoke,
                    "telegram update-stream lock is not held by this registration".to_owned(),
                );
            }
            self.ensure_poller_started_locked(&mut state);
            state.registered_agents.insert(invoke.agent_id.clone());
            if was_unregistered {
                state.poller_drained_initial_backlog = false;
            }
            state
                .agent_labels
                .entry(invoke.agent_id.clone())
                .or_insert_with(|| invoke.agent_id.to_string());
            state.mark_coordination_changed();
            self.state.notify_all();
        } else {
            state.registered_agents.remove(&invoke.agent_id);
            state
                .selected_agent_by_chat
                .retain(|_, agent| agent != &invoke.agent_id);
            if state.registered_agents.is_empty() {
                state.poller_drained_initial_backlog = false;
            }
            state.mark_coordination_changed();
            self.state.notify_all();
        }
        tool_result(
            invoke,
            if enabled {
                "registered for Telegram messages"
            } else {
                "unregistered from Telegram messages"
            },
        )
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
                    "telegram_register could not verify Telegram webhook status before polling; \
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
        self.output.emit(Event::HarnessNotice(HarnessNotice {
            kind: tau_proto::notice_kind::EXTENSION_NOTICE.to_owned(),
            message: message.to_owned(),
            level: NoticeLevel::Warning,
            always_show: false,
        }));
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
        std::thread::spawn(move || {
            poll_loop_with_tool_names(state_arc, client, output, shutdown, tool_names);
        });
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
            Ok(()) => tool_result(invoke, "sent Telegram message"),
            Err(message) => tool_error(invoke, message),
        }
    }

    fn process_update_for_generation(&self, update: TgUpdate, config_generation: ConfigGeneration) {
        let Some(message) = update.message else {
            return;
        };
        let Some(cfg) = self.config_for_allowed_message(&message, config_generation) else {
            return;
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
            return;
        }
        let Some(text) = self.trimmed_message_text(&cfg, &message, config_generation) else {
            return;
        };
        let (command, rest) = parse_command(&text);
        if self.rejects_unlinked_command(&cfg, &message, active_chat, command, config_generation) {
            return;
        }
        if self.handle_command(
            &cfg,
            &message,
            is_private_chat,
            command,
            rest,
            config_generation,
        ) {
            return;
        }

        self.route_plain_text(&cfg, &message, &text, config_generation);
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
            tracing::warn!(target: LOG_TARGET, user_id = message.user_id, "ignoring Telegram message from unallowed user");
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
        is_private_chat: bool,
        command: Option<&str>,
        rest: &str,
        config_generation: ConfigGeneration,
    ) -> bool {
        match command {
            Some("/start") => {
                self.handle_start_command(cfg, message, is_private_chat, config_generation);
                true
            }
            Some("/agents") => {
                self.handle_agents_command(cfg, message.chat_id, config_generation);
                true
            }
            Some("/select") => {
                self.handle_select_command(cfg, message.chat_id, rest, config_generation);
                true
            }
            Some("/to") => {
                self.handle_to_command(cfg, message, rest, config_generation);
                true
            }
            Some(_) => {
                self.reply(
                    cfg,
                    message.chat_id,
                    "Unknown Telegram command. Supported commands: /start, /agents, /select, /to.",
                    config_generation,
                );
                true
            }
            None => false,
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
        rest: &str,
        config_generation: ConfigGeneration,
    ) {
        let (target, body) = split_first(rest);
        if target.is_empty() || body.trim().is_empty() {
            self.reply(
                cfg,
                message.chat_id,
                "Usage: /to <agent-id-or-prefix> <message>",
                config_generation,
            );
            return;
        }

        match self.resolve_registered_agent(target) {
            Ok(agent_id) => self.route_text(message, agent_id, body.trim(), config_generation),
            Err(reply) => {
                self.reply(cfg, message.chat_id, &reply, config_generation);
            }
        }
    }

    fn route_plain_text(
        &self,
        cfg: &RuntimeConfig,
        message: &TgMessage,
        text: &str,
        config_generation: ConfigGeneration,
    ) {
        match self.plain_text_target(message.chat_id) {
            Ok(agent_id) => self.route_text(message, agent_id, text, config_generation),
            Err(reply) => {
                self.reply(cfg, message.chat_id, &reply, config_generation);
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
        message: &TgMessage,
        agent_id: AgentId,
        text: &str,
        config_generation: ConfigGeneration,
    ) {
        if !self.poll_response_matches_config(config_generation) {
            return;
        }
        let source = message
            .from_name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| message.user_id.to_string());
        let prompt = format!("[telegram from {source}] {text}");
        self.output
            .emit(Event::ExtPromptSubmitRequest(ExtPromptSubmitRequest {
                agent_id,
                text: prompt,
                ctx_id: None,
            }));
    }
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
        self.request_shutdown();
    }
}

fn wait_for_poller_ready_or_shutdown(
    state_cell: &SharedState,
    shutdown: &AtomicBool,
) -> Option<PollRequest> {
    let mut state = state_cell.lock();
    if state.registered_agents.is_empty() {
        state.update_stream_lock = None;
    }
    state = state_cell.wait_while(state, |state| {
        if state.registered_agents.is_empty() {
            state.update_stream_lock = None;
        }
        !state.shutdown_requested
            && (state.config.is_none()
                || state.registered_agents.is_empty()
                || state.update_stream_lock.is_none())
    });
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
    observed_generation: u64,
) {
    if delay.is_zero() || shutdown.load(Ordering::Relaxed) {
        return;
    }
    let state = state_cell.lock();
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
    poll_loop_with_tool_names(state, client, output, shutdown, ToolNames::legacy());
}

fn poll_loop_with_tool_names(
    state: Arc<SharedState>,
    client: Arc<dyn TelegramClient>,
    output: Output,
    shutdown: Arc<AtomicBool>,
    tool_names: ToolNames,
) {
    let ext = Extension {
        state,
        client,
        output,
        shutdown: Arc::clone(&shutdown),
        tool_names,
    };
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        let Some(poll_request) = wait_for_poller_ready_or_shutdown(&ext.state, &shutdown) else {
            return;
        };
        if !poll_request.update_stream_lock.covers(&poll_request.cfg) {
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
        match ext.client.get_updates(&request_cfg, poll_request.offset) {
            Ok(updates) => {
                if shutdown.load(Ordering::Relaxed) {
                    return;
                }
                if !ext.poll_response_matches_config(poll_request.config_generation) {
                    continue;
                }
                let mut stale_generation = false;
                let draining = {
                    let mut state = ext.state.lock();
                    if state.config_generation != poll_request.config_generation
                        || state.config.is_none()
                    {
                        stale_generation = true;
                        false
                    } else if !state.poller_drained_initial_backlog {
                        if let Some(max_update_id) = updates.iter().map(|u| u.update_id).max() {
                            state.next_update_offset = Some(max_update_id + 1);
                        }
                        if updates.is_empty() {
                            state.poller_drained_initial_backlog = true;
                        }
                        true
                    } else {
                        false
                    }
                };
                if stale_generation {
                    continue;
                }
                if draining {
                    continue;
                }
                if updates.is_empty() {
                    wait_for_coordination_change_or_shutdown(
                        &ext.state,
                        &shutdown,
                        Duration::from_millis(50),
                        poll_request.coordination_generation,
                    );
                }
                for update in updates {
                    {
                        let mut state = ext.state.lock();
                        if state.config_generation != poll_request.config_generation
                            || state.config.is_none()
                        {
                            break;
                        }
                        state.next_update_offset = Some(update.update_id + 1);
                    }
                    ext.process_update_for_generation(update, poll_request.config_generation);
                }
            }
            Err(message) => {
                if !ext.poll_response_matches_config(poll_request.config_generation) {
                    continue;
                }
                if let Some(diagnostic) = telegram_contention_diagnostic(&message) {
                    tracing::warn!(target: LOG_TARGET, error = %message, "telegram update stream contention detected");
                    ext.fail_active_polling_with_notice(&poll_request.cfg, &diagnostic);
                    continue;
                }
                tracing::warn!(target: LOG_TARGET, error = %message, "telegram polling failed");
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

fn run_with_client<R, W>(
    reader: R,
    writer: W,
    client: Arc<dyn TelegramClient>,
) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    let mut runtime = tau_client::TauExtensionRunner::new(TelegramExtension)
        .start_manual_loop_deferred_startup_with_state(reader, writer, move |handle| {
            TelegramRuntime {
                ext: Extension::new(client, handle),
            }
        })?;
    let Some(configure) = read_initial_config(&mut runtime)? else {
        let state = runtime.finish()?;
        state.ext.request_shutdown();
        return Ok(());
    };
    match configure_tool_names(&configure, runtime.state_mut()) {
        Ok(tool_names) => {
            send_startup_declarations(&mut runtime, &tool_names)?;
        }
        Err(error) => {
            runtime.state().ext.clear_config_after_error();
            runtime.handle().config_error(error.to_string())?;
            runtime.startup_ready(Some("telegram disabled".to_owned()))?;
        }
    }
    let exit = drive_manual_runtime(&mut runtime)?;
    runtime.state().ext.request_shutdown();
    let state = match exit {
        ManualRuntimeExit::Disconnect => runtime.finish_detached(),
        ManualRuntimeExit::InputClosed => runtime.finish()?,
    };
    state.ext.request_shutdown();
    Ok(())
}

struct TelegramExtension;

impl TauExtension for TelegramExtension {
    type State = TelegramRuntime;

    fn name(&self) -> &'static str {
        "tau-ext-telegram"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        // Tool declarations and subscriptions are emitted manually after the
        // initial Configure message because Telegram tool names are
        // configuration/instance dependent.
        let _ = builder;
    }
}

struct TelegramRuntime {
    /// Shared Telegram bridge state and background-worker coordination.
    ext: Extension,
}

/// Read the mandatory initial configuration used for namespaced startup
/// declarations.
fn read_initial_config(
    runtime: &mut tau_client::ManualExtensionRuntime<TelegramRuntime>,
) -> Result<Option<tau_proto::Configure>, Box<dyn Error>> {
    loop {
        match runtime.recv()? {
            ManualRuntimeInput::Message(tau_proto::HarnessOutputMessage::Configure(configure)) => {
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
    runtime: &mut TelegramRuntime,
) -> Result<ToolNames, Box<dyn Error>> {
    let parsed = parse_ext_config(&configure.config)?;
    let tool_names =
        ToolNames::from_config_and_instance(&parsed, configure.instance_name.as_ref())?;
    let runtime_cfg = parsed.validate(&configure.secrets)?;
    runtime.ext.tool_names = tool_names.clone();
    runtime
        .ext
        .apply_config(runtime_cfg, configure.state_dir.clone())?;
    Ok(tool_names)
}

/// Publish instance-specific tool names and subscriptions before Ready.
fn send_startup_declarations(
    runtime: &mut tau_client::ManualExtensionRuntime<TelegramRuntime>,
    tool_names: &ToolNames,
) -> ClientResult<()> {
    runtime.startup_subscribe([
        tau_proto::EventSelector::Exact(tau_proto::EventName::TOOL_STARTED),
        tau_proto::EventSelector::Exact(tau_proto::EventName::AGENT_DISPLAY_NAME_SET),
        tau_proto::EventSelector::Exact(tau_proto::EventName::AGENT_STARTED),
        tau_proto::EventSelector::Exact(tau_proto::EventName::SESSION_AGENT_UNLOADED),
        tau_proto::EventSelector::Exact(tau_proto::EventName::SESSION_SHUTDOWN),
    ])?;
    runtime.startup_event(Event::ToolRegister(tau_proto::ToolRegister {
        tool: register_tool_spec_for(tool_names),
        tool_group: Some(telegram_tool_group_for(tool_names)),
        prompt_fragment: None,
    }))?;
    runtime.startup_event(Event::ToolRegister(tau_proto::ToolRegister {
        tool: send_tool_spec_for(tool_names),
        tool_group: Some(telegram_tool_group_for(tool_names)),
        prompt_fragment: None,
    }))?;
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
) -> Result<ManualRuntimeExit, Box<dyn Error>> {
    loop {
        match runtime.recv()? {
            ManualRuntimeInput::Message(tau_proto::HarnessOutputMessage::Configure(configure)) => {
                handle_configure_message(runtime.state_mut(), configure);
            }
            ManualRuntimeInput::Message(tau_proto::HarnessOutputMessage::Deliver(delivery)) => {
                if delivery.replay {
                    continue;
                }
                match *delivery.event {
                    Event::ToolStarted(invoke)
                        if runtime.state().ext.handles_tool(invoke.tool_name.as_str()) =>
                    {
                        runtime.state().ext.dispatch_tool(invoke);
                    }
                    Event::ToolStarted(_) => {}
                    event => handle_live_event_value(runtime.state(), event),
                }
            }
            ManualRuntimeInput::Message(tau_proto::HarnessOutputMessage::Disconnect(_)) => {
                break Ok(ManualRuntimeExit::Disconnect);
            }
            ManualRuntimeInput::InputClosed => break Ok(ManualRuntimeExit::InputClosed),
            ManualRuntimeInput::Timeout => {}
            ManualRuntimeInput::Message(_) => {}
        }
    }
}

/// Apply a runtime reconfiguration and report errors explicitly to the harness.
fn handle_configure_message(runtime: &mut TelegramRuntime, configure: tau_proto::Configure) {
    let result = parse_ext_config(&configure.config)
        .and_then(|cfg| {
            let requested = ToolNames::from_config_and_instance(&cfg, configure.instance_name.as_ref())?;
            if requested != runtime.ext.tool_names {
                return Err(
                    "telegram tool namespace cannot change after extension startup; restart the extension to change tool names"
                        .to_owned(),
                );
            }
            cfg.validate(&configure.secrets)
        })
        .and_then(|cfg| runtime.ext.apply_config(cfg, configure.state_dir));
    if let Err(message) = result {
        runtime.ext.clear_config_after_error();
        if let Output::Client(handle) = &runtime.ext.output {
            let _ = handle.config_error(message);
        }
    }
}

/// Handle a delivered live event without tau-client's static handler registry.
fn handle_live_event_value(runtime: &TelegramRuntime, event: Event) {
    match event {
        Event::AgentDisplayNameSet(name) => {
            let mut state = runtime.ext.state.lock();
            state
                .agent_labels
                .insert(name.agent_id.clone(), name.display_name.clone());
        }
        Event::AgentStarted(started) => {
            if let Some(display_name) = started.display_name.clone() {
                let mut state = runtime.ext.state.lock();
                state.agent_labels.insert(started.agent_id, display_name);
            }
        }
        Event::SessionAgentUnloaded(unloaded) => {
            let mut state = runtime.ext.state.lock();
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
        }
        Event::SessionShutdown(_) => {
            let mut state = runtime.ext.state.lock();
            state.registered_agents.clear();
            state.agent_labels.clear();
            state.selected_agent_by_chat.clear();
            state.poller_drained_initial_backlog = false;
            state.update_stream_lock = None;
            state.shutdown_requested = true;
            state.mark_coordination_changed();
            runtime.ext.shutdown.store(true, Ordering::Relaxed);
            runtime.ext.state.notify_all();
        }
        _ => {}
    }
}

fn parse_ext_config(value: &CborValue) -> Result<ExtConfig, String> {
    value.deserialized().map_err(|e| e.to_string())
}

fn namespace_from_instance_name(instance_name: &str) -> Result<String, String> {
    let mut namespace = String::with_capacity(instance_name.len());
    for byte in instance_name.bytes() {
        match byte {
            b'_' => namespace.push_str("__"),
            b'-' => namespace.push_str("_d"),
            byte if byte.is_ascii_alphanumeric() => namespace.push(byte as char),
            _ => {
                return Err(
                    "telegram extension instance names may contain only ASCII letters, digits, '_' and '-'"
                        .to_owned(),
                );
            }
        }
    }
    validate_tool_namespace(&namespace)?;
    Ok(namespace)
}

fn validate_tool_namespace(namespace: &str) -> Result<(), String> {
    if namespace.is_empty() {
        return Err("telegram `tool_namespace` must not be empty".to_owned());
    }
    if namespace.len() > tau_proto::ToolName::MAX_LEN.saturating_sub("_register".len()) {
        return Err("telegram `tool_namespace` is too long".to_owned());
    }
    if tau_proto::ToolName::try_new(namespace).is_none() {
        return Err(
            "telegram `tool_namespace` may contain only ASCII letters, digits, and '_'".to_owned(),
        );
    }
    Ok(())
}

#[cfg(test)]
fn telegram_tool_group() -> tau_proto::ToolGroup {
    telegram_tool_group_for(&ToolNames::legacy())
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
    register_tool_spec_for(&ToolNames::legacy())
}

fn register_tool_spec_for(tool_names: &ToolNames) -> ToolSpec {
    ToolSpec {
        name: tool_names.register.clone(),
        model_visible_name: Some(tool_names.register.clone()),
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
    send_tool_spec_for(&ToolNames::legacy())
}

fn send_tool_spec_for(tool_names: &ToolNames) -> ToolSpec {
    ToolSpec {
        name: tool_names.send.clone(),
        model_visible_name: Some(tool_names.send.clone()),
        description: Some(format!(
            "Send a text message to the configured or linked Telegram chat for the `{}` bot namespace. Only registered agents may use this tool; it cannot choose arbitrary chat ids. Use it to answer prompts prefixed with [telegram from ...].",
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
        let tls_config = ureq::tls::TlsConfig::builder()
            .root_certs(ureq::tls::RootCerts::PlatformVerifier)
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
    fn get_webhook_info(&self, cfg: &RuntimeConfig) -> Result<TgWebhookInfo, String> {
        let value = self.post(cfg, "getWebhookInfo", serde_json::json!({}))?;
        decode_webhook_info(&value)
    }

    fn get_updates(
        &self,
        cfg: &RuntimeConfig,
        offset: Option<i64>,
    ) -> Result<Vec<TgUpdate>, String> {
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
            .ok_or_else(|| "Telegram getUpdates response missing result array".to_owned())?;
        Ok(result.iter().filter_map(decode_update).collect())
    }

    fn send_message(&self, cfg: &RuntimeConfig, chat_id: i64, text: &str) -> Result<(), String> {
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
    ) -> Result<serde_json::Value, String> {
        let url = format!("{}/bot{}/{}", cfg.api_base, cfg.bot_token, method);
        let mut response = self
            .agent
            .post(&url)
            .content_type("application/json")
            .send(body.to_string())
            .map_err(|_e| "Telegram transport error".to_owned())?;
        let status = response.status();
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|e| format!("reading Telegram response: {e}"))?;
        let text = redact_token(&text, &cfg.bot_token);
        if !status.is_success() {
            return Err(format!(
                "Telegram returned HTTP {}: {text}",
                status.as_u16()
            ));
        }
        serde_json::from_str(&text).map_err(|e| format!("invalid Telegram JSON: {e}"))
    }
}

fn redact_token(text: &str, token: &str) -> String {
    if token.is_empty() {
        text.to_owned()
    } else {
        text.replace(token, "<redacted>")
    }
}

fn decode_webhook_info(value: &serde_json::Value) -> Result<TgWebhookInfo, String> {
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
        .map(str::to_owned);
    Ok(TgWebhookInfo {
        url,
        pending_update_count,
        last_error_message,
    })
}

fn webhook_active_message(info: &TgWebhookInfo) -> String {
    let mut message = "Telegram bot has an active webhook, so getUpdates polling cannot be used. \
                       Tau did not delete the webhook or drop updates; remove the webhook yourself \
                       or configure a different bot token."
        .to_owned();
    if let Some(count) = info.pending_update_count {
        message.push_str(&format!(" Telegram reports {count} pending update(s)."));
    }
    if let Some(error) = info
        .last_error_message
        .as_deref()
        .filter(|error| !error.trim().is_empty())
    {
        message.push_str(" Last webhook error: ");
        message.push_str(&bounded_diagnostic_text(error));
    }
    message
}

fn bounded_diagnostic_text(text: &str) -> String {
    let mut sanitized = String::new();
    for ch in text.trim().chars() {
        let ch_len = ch.len_utf8();
        if sanitized.len() + ch_len > MAX_DIAGNOSTIC_TEXT_BYTES {
            sanitized.push('…');
            break;
        }
        if ch.is_control() && ch != '\n' && ch != '\t' {
            sanitized.push('�');
        } else {
            sanitized.push(ch);
        }
    }
    sanitized
}

fn telegram_contention_diagnostic(message: &str) -> Option<String> {
    let lower = message.to_ascii_lowercase();
    if !lower.contains("http 409") && !lower.contains("conflict") {
        return None;
    }
    if lower.contains("webhook") {
        return Some(
            "Telegram getUpdates returned HTTP 409 because a webhook is active or was changed. \
             Tau stopped Telegram polling for this registration; it did not delete the webhook \
             or drop updates. Remove the webhook yourself or configure a different bot token."
                .to_owned(),
        );
    }
    if lower.contains("getupdates") || lower.contains("bot instance") {
        return Some(
            "Telegram getUpdates returned HTTP 409 because another long-poll consumer is using \
             this bot token. Tau stopped Telegram polling for this registration to avoid racing \
             the singleton update stream; stop the other bot/session or configure a different \
             bot token."
                .to_owned(),
        );
    }
    Some(
        "Telegram getUpdates returned HTTP 409 conflict. Tau stopped Telegram polling for this \
         registration because the bot update stream is not exclusively available."
            .to_owned(),
    )
}

fn decode_update(value: &serde_json::Value) -> Option<TgUpdate> {
    let update_id = value.get("update_id")?.as_i64()?;
    let message = decode_message(value);
    Some(TgUpdate { update_id, message })
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
