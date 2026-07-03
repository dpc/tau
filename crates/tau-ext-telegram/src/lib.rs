//! Personal Telegram bridge extension for Tau agents.
//!
//! The extension exposes `telegram_register` and `telegram_send` tools. It
//! keeps listener registrations in memory and uses the Telegram Bot API only
//! after an agent registers or another Telegram action needs the client.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use tau_client::{ClientError, ClientHandle, ClientResult, ExtensionBuilder, TauExtension};
use tau_proto::{
    AgentId, CborValue, Event, ExtPromptSubmitRequest, HarnessInputMessage, ToolError, ToolExample,
    ToolProgress, ToolResult, ToolSpec, ToolStarted, ToolUseState, ToolUseStatus,
};

/// Tracing target used by this extension.
pub const LOG_TARGET: &str = "telegram";

/// Internal tool name for registering the current agent as a Telegram listener.
pub const REGISTER_TOOL_NAME: &str = "telegram_register";

/// Internal tool name for sending a Telegram message from a registered agent.
pub const SEND_TOOL_NAME: &str = "telegram_send";

/// Tool group name shared by all Telegram bridge tools.
pub const TOOL_GROUP_NAME: &str = "telegram";

/// Tag marking tools that register an agent with the Telegram bridge.
pub const REGISTER_TOOL_TAG: &str = "telegram:register";

/// Tag marking tools that send messages through the Telegram bridge.
pub const SEND_TOOL_TAG: &str = "telegram:send";

const DEFAULT_API_BASE: &str = "https://api.telegram.org";
const DEFAULT_POLL_TIMEOUT_SECONDS: u64 = 25;
const HTTP_TIMEOUT: Duration = Duration::from_secs(35);

/// Run the Telegram extension over stdio.
pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    tau_client::init_logging_for(LOG_TARGET);
    run(std::io::stdin(), std::io::stdout())
}

/// Run the Telegram extension over an arbitrary transport.
pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write + Send + 'static,
{
    run_with_client(reader, writer, Arc::new(HttpTelegramClient::default()))
}

/// Small Bot API surface used by the extension and faked by unit tests.
trait TelegramClient: Send + Sync + 'static {
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

/// Raw deserialized extension config from `harness.yaml`.
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ExtConfig {
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ConfigGeneration(u64);

impl ConfigGeneration {
    fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }
}

#[derive(Default)]
struct State {
    config: Option<RuntimeConfig>,
    config_generation: ConfigGeneration,
    registered_agents: HashSet<AgentId>,
    agent_labels: HashMap<AgentId, String>,
    selected_agent_by_chat: HashMap<i64, AgentId>,
    learned_chat: Option<LinkedChat>,
    poller_started: bool,
    poller_drained_initial_backlog: bool,
    next_update_offset: Option<i64>,
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
    state: Arc<Mutex<State>>,
    client: Arc<dyn TelegramClient>,
    output: Output,
    shutdown: Arc<AtomicBool>,
}

impl Extension {
    fn new(client: Arc<dyn TelegramClient>, output: impl Into<Output>) -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            client,
            output: output.into(),
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    fn apply_config(&self, cfg: RuntimeConfig, _state_dir: Option<std::path::PathBuf>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.config_generation = state.config_generation.next();
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
        }
        state.config = Some(cfg);
    }

    fn clear_config_after_error(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.config_generation = state.config_generation.next();
        state.config = None;
        state.registered_agents.clear();
        state.selected_agent_by_chat.clear();
        state.learned_chat = None;
        state.poller_drained_initial_backlog = false;
        state.next_update_offset = None;
    }

    fn poll_response_matches_config(&self, config_generation: ConfigGeneration) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
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
            REGISTER_TOOL_NAME => self.handle_register(invoke),
            SEND_TOOL_NAME => self.handle_send(invoke),
            _ => tool_error(invoke, "unknown telegram tool".to_owned()),
        };
        self.output.emit(event);
    }

    fn handle_register(&self, invoke: ToolStarted) -> Event {
        if let Err(message) = validate_object_fields(&invoke.arguments, &["enabled"]) {
            return tool_error(invoke, message);
        }
        let enabled = match cbor_bool_field(&invoke.arguments, "enabled") {
            Ok(enabled) => enabled,
            Err(message) => return tool_error(invoke, message),
        };
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if enabled {
            state.registered_agents.insert(invoke.agent_id.clone());
            state
                .agent_labels
                .entry(invoke.agent_id.clone())
                .or_insert_with(|| invoke.agent_id.to_string());
            if let Err(message) = self.ensure_poller_started_locked(&mut state) {
                return tool_error(invoke, message);
            }
        } else {
            state.registered_agents.remove(&invoke.agent_id);
            state
                .selected_agent_by_chat
                .retain(|_, agent| agent != &invoke.agent_id);
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

    fn ensure_poller_started_locked(&self, state: &mut State) -> Result<(), String> {
        if state.poller_started {
            return Ok(());
        }
        if state.config.is_none() {
            return Err("telegram extension is not configured".to_owned());
        }
        state.poller_started = true;
        let state_arc = Arc::clone(&self.state);
        let output = self.output.clone();
        let client = Arc::clone(&self.client);
        let shutdown = Arc::clone(&self.shutdown);
        std::thread::spawn(move || poll_loop(state_arc, client, output, shutdown));
        Ok(())
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
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if !state.registered_agents.contains(&invoke.agent_id) {
                return tool_error(
                    invoke,
                    "telegram_send requires telegram_register(enabled: true) first".to_owned(),
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
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
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
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
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
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
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
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
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
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
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
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
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
            Err("No Tau agents are registered. Ask an agent to call telegram_register(enabled: true).".to_owned())
        } else {
            Err(
                "Multiple Tau agents are registered. Use /agents then /select <agent-id-or-prefix>."
                    .to_owned(),
            )
        }
    }

    fn resolve_registered_agent(&self, target: &str) -> Result<AgentId, String> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
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

fn sleep_interruptibly(shutdown: &AtomicBool, total: Duration) {
    let step = Duration::from_millis(100);
    let mut slept = Duration::ZERO;
    while slept < total && !shutdown.load(Ordering::Relaxed) {
        std::thread::sleep(step);
        slept += step;
    }
}

impl Drop for Extension {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

fn poll_loop(
    state: Arc<Mutex<State>>,
    client: Arc<dyn TelegramClient>,
    output: Output,
    shutdown: Arc<AtomicBool>,
) {
    let ext = Extension {
        state,
        client,
        output,
        shutdown: Arc::clone(&shutdown),
    };
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        let (cfg, offset, config_generation) = {
            let state = ext.state.lock().unwrap_or_else(|e| e.into_inner());
            let Some(cfg) = state.config.clone() else {
                drop(state);
                sleep_interruptibly(&shutdown, Duration::from_millis(50));
                continue;
            };
            (cfg, state.next_update_offset, state.config_generation)
        };
        let mut request_cfg = cfg.clone();
        let draining_initial_backlog = {
            let state = ext.state.lock().unwrap_or_else(|e| e.into_inner());
            !state.poller_drained_initial_backlog
        };
        if draining_initial_backlog {
            request_cfg.poll_timeout_seconds = 0;
        }
        match ext.client.get_updates(&request_cfg, offset) {
            Ok(updates) => {
                if shutdown.load(Ordering::Relaxed) {
                    return;
                }
                if !ext.poll_response_matches_config(config_generation) {
                    continue;
                }
                let mut stale_generation = false;
                let draining = {
                    let mut state = ext.state.lock().unwrap_or_else(|e| e.into_inner());
                    if state.config_generation != config_generation || state.config.is_none() {
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
                    std::thread::sleep(Duration::from_millis(50));
                }
                for update in updates {
                    {
                        let mut state = ext.state.lock().unwrap_or_else(|e| e.into_inner());
                        if state.config_generation != config_generation || state.config.is_none() {
                            break;
                        }
                        state.next_update_offset = Some(update.update_id + 1);
                    }
                    ext.process_update_for_generation(update, config_generation);
                }
            }
            Err(message) => {
                if !ext.poll_response_matches_config(config_generation) {
                    continue;
                }
                tracing::warn!(target: LOG_TARGET, error = %message, "telegram polling failed");
                sleep_interruptibly(&shutdown, Duration::from_secs(5));
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
    R: Read,
    W: Write + Send + 'static,
{
    let state = tau_client::TauExtensionRunner::new(TelegramExtension)
        .run_detached_writer_with_state(reader, writer, move |handle| TelegramRuntime {
            ext: Extension::new(client, handle),
        })?;
    state.ext.shutdown.store(true, Ordering::Relaxed);
    Ok(())
}

struct TelegramExtension;

impl TauExtension for TelegramExtension {
    type State = TelegramRuntime;

    fn name(&self) -> &'static str {
        "tau-ext-telegram"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder
            .configure_with_error::<ExtConfig>(
                |cx| {
                    let secrets = cx.secrets().clone();
                    let state_dir = cx.state_dir().map(std::path::Path::to_path_buf);
                    let cfg = cx.config.validate(&secrets).map_err(ClientError::handler)?;
                    cx.state.ext.apply_config(cfg, state_dir);
                    Ok(())
                },
                |cx| {
                    cx.state.ext.clear_config_after_error();
                },
            )
            .tool_with_group_and_prompt_fragment(
                register_tool_spec(),
                Some(telegram_tool_group()),
                None,
                handle_tool_invocation,
            )
            .tool_with_group_and_prompt_fragment(
                send_tool_spec(),
                Some(telegram_tool_group()),
                None,
                handle_tool_invocation,
            )
            .on_raw_live(
                tau_proto::EventSelector::Exact(tau_proto::EventName::AGENT_DISPLAY_NAME_SET),
                handle_live_event,
            )
            .on_raw_live(
                tau_proto::EventSelector::Exact(tau_proto::EventName::AGENT_STARTED),
                handle_live_event,
            )
            .on_raw_live(
                tau_proto::EventSelector::Exact(tau_proto::EventName::SESSION_AGENT_UNLOADED),
                handle_live_event,
            )
            .on_raw_live(
                tau_proto::EventSelector::Exact(tau_proto::EventName::SESSION_SHUTDOWN),
                handle_live_event,
            )
            .ready_message("telegram ready");
    }
}

struct TelegramRuntime {
    /// Shared Telegram bridge state and background-worker coordination.
    ext: Extension,
}

fn handle_tool_invocation(cx: tau_client::ToolContext<'_, TelegramRuntime>) -> ClientResult<()> {
    cx.state.ext.dispatch_tool(cx.invoke().clone());
    Ok(())
}

fn handle_live_event(cx: tau_client::RawEventContext<'_, TelegramRuntime>) -> ClientResult<()> {
    match cx.event() {
        Event::AgentDisplayNameSet(name) => {
            let mut state = cx.state.ext.state.lock().unwrap_or_else(|e| e.into_inner());
            state
                .agent_labels
                .insert(name.agent_id.clone(), name.display_name.clone());
        }
        Event::AgentStarted(started) => {
            if let Some(display_name) = started.display_name.clone() {
                let mut state = cx.state.ext.state.lock().unwrap_or_else(|e| e.into_inner());
                state
                    .agent_labels
                    .insert(started.agent_id.clone(), display_name);
            }
        }
        Event::SessionAgentUnloaded(unloaded) => {
            let mut state = cx.state.ext.state.lock().unwrap_or_else(|e| e.into_inner());
            state.registered_agents.remove(&unloaded.agent_id);
            state.agent_labels.remove(&unloaded.agent_id);
            state
                .selected_agent_by_chat
                .retain(|_, agent_id| agent_id != &unloaded.agent_id);
        }
        Event::SessionShutdown(_) => {
            let mut state = cx.state.ext.state.lock().unwrap_or_else(|e| e.into_inner());
            state.registered_agents.clear();
            state.agent_labels.clear();
            state.selected_agent_by_chat.clear();
        }
        _ => {}
    }
    Ok(())
}

fn telegram_tool_group() -> tau_proto::ToolGroup {
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
        description: Some(
            "Register or unregister this agent for Telegram messages. Use enabled=true to allow an allowlisted Telegram user to send prompts to this agent; use enabled=false to stop listening. When replying to Telegram-originated prompts, use telegram_send."
                .to_owned(),
        ),
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

fn send_tool_spec() -> ToolSpec {
    ToolSpec {
        name: tau_proto::ToolName::new(SEND_TOOL_NAME),
        model_visible_name: Some(tau_proto::ToolName::new(SEND_TOOL_NAME)),
        description: Some(
            "Send a text message to the configured or linked Telegram chat. Only registered agents may use this tool; it cannot choose arbitrary chat ids. Use it to answer prompts prefixed with [telegram from ...]."
                .to_owned(),
        ),
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
            note: Some("There is no chat_id argument; the configured or linked chat is used.".to_owned()),
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
