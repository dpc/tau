//! Personal Slack Socket Mode bridge extension for Tau agents.
//!
//! The extension exposes `slack_register` and `slack_send` tools. It is
//! disabled by default, requires Slack token secrets plus a non-empty
//! allowlist, and treats Slack text as external untrusted prompt input.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::error::Error;
use std::io::{BufReader, BufWriter, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tau_proto::{
    AgentId, CborValue, ConfigError, Event, ExtPromptSubmitRequest, HarnessInputMessage,
    HarnessNotice, HarnessOutputMessage, NoticeLevel, PeerInputReader, PeerOutputWriter, ToolError,
    ToolExample, ToolProgress, ToolResult, ToolSpec, ToolStarted, ToolUseState, ToolUseStatus,
};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// Tracing target used by this extension.
pub const LOG_TARGET: &str = "slack";

/// Internal tool name for registering the current agent as a Slack listener.
pub const REGISTER_TOOL_NAME: &str = "slack_register";

/// Internal tool name for sending a Slack message from a registered agent.
pub const SEND_TOOL_NAME: &str = "slack_send";

/// Tool group name shared by all Slack bridge tools.
pub const TOOL_GROUP_NAME: &str = "slack";

/// Tag marking tools that register an agent with the Slack bridge.
pub const REGISTER_TOOL_TAG: &str = "slack:register";

/// Tag marking tools that send messages through the Slack bridge.
pub const SEND_TOOL_TAG: &str = "slack:send";

const DEFAULT_API_BASE: &str = "https://slack.com/api";
const DEFAULT_MAX_MESSAGE_BYTES: usize = 16 * 1024;
const MAX_MESSAGE_BYTES: usize = 128 * 1024;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const DUPLICATE_CACHE_SIZE: usize = 1024;
const MAX_DIAGNOSTIC_BYTES: usize = 512;
const MAX_SOCKET_FRAME_BYTES: usize = 256 * 1024;
const INITIAL_RECONNECT_BACKOFF: Duration = Duration::from_secs(1);
const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(30);

/// Run the Slack extension over stdio.
pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    tau_extension::init_logging_for(LOG_TARGET);
    run(std::io::stdin(), std::io::stdout())
}

/// Run the Slack extension over an arbitrary transport.
pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write + Send + 'static,
{
    run_with_client(reader, writer, Arc::new(HttpSlackClient::default()))
}

/// Small Slack Web API surface used by the extension and faked by unit tests.
trait SlackClient: Send + Sync + 'static {
    /// Open a Socket Mode websocket URL with the configured app token.
    fn open_socket(&self, cfg: &RuntimeConfig) -> Result<String, String>;

    /// Return the bot user id from `auth.test` using the configured bot token.
    fn auth_test(&self, cfg: &RuntimeConfig) -> Result<String, String>;

    /// Send a plain text message to one configured or linked Slack
    /// conversation.
    fn post_message(&self, cfg: &RuntimeConfig, channel_id: &str, text: &str)
    -> Result<(), String>;
}

/// Validated runtime configuration, including resolved secret values.
#[derive(Clone)]
struct RuntimeConfig {
    /// Resolved Slack app-level token. Never log this value.
    app_token: String,
    /// Resolved Slack bot token. Never log this value.
    bot_token: String,
    /// Slack user ids allowed to interact with this bridge.
    allowed_user_ids: HashSet<String>,
    /// Optional fixed Slack conversation for routing and outgoing messages.
    configured_channel_id: Option<String>,
    /// Slack Web API base URL.
    api_base: String,
    /// Maximum accepted inbound or outbound text size in bytes.
    max_message_bytes: usize,
}

/// Raw deserialized extension config from `harness.yaml`.
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ExtConfig {
    /// Secret name carrying the Slack app-level Socket Mode token.
    app_token_secret: Option<String>,
    /// Secret name carrying the Slack bot token.
    bot_token_secret: Option<String>,
    /// Slack user ids allowed to drive Tau agents.
    allowed_user_ids: Vec<String>,
    /// Optional fixed Slack conversation id for messages.
    channel_id: Option<String>,
    /// Optional Slack Web API base URL, mostly for tests.
    api_base: Option<String>,
    /// Optional maximum accepted text size in bytes.
    max_message_bytes: Option<usize>,
}

impl ExtConfig {
    /// Validate raw config and resolve referenced Tau secrets.
    fn validate(
        self,
        secrets: &BTreeMap<String, tau_proto::SecretValue>,
    ) -> Result<RuntimeConfig, String> {
        let app_secret = self
            .app_token_secret
            .ok_or_else(|| "slack config requires `app_token_secret`".to_owned())?;
        let bot_secret = self
            .bot_token_secret
            .ok_or_else(|| "slack config requires `bot_token_secret`".to_owned())?;
        let app_token = secret_value(secrets, &app_secret, "app")?;
        let bot_token = secret_value(secrets, &bot_secret, "bot")?;
        if self.allowed_user_ids.is_empty() {
            return Err("slack config requires non-empty `allowed_user_ids`".to_owned());
        }
        let mut allowed_user_ids = HashSet::new();
        for user_id in self.allowed_user_ids {
            let user_id = validate_slack_id("allowed_user_ids", &user_id)?;
            allowed_user_ids.insert(user_id);
        }
        let configured_channel_id = self
            .channel_id
            .map(|id| validate_slack_id("channel_id", &id))
            .transpose()?;
        let api_base = self
            .api_base
            .unwrap_or_else(|| DEFAULT_API_BASE.to_owned())
            .trim_end_matches('/')
            .to_owned();
        validate_api_base(&api_base)?;
        let max_message_bytes = self.max_message_bytes.unwrap_or(DEFAULT_MAX_MESSAGE_BYTES);
        if max_message_bytes == 0 || max_message_bytes > MAX_MESSAGE_BYTES {
            return Err(format!(
                "slack `max_message_bytes` must be between 1 and {MAX_MESSAGE_BYTES}"
            ));
        }
        Ok(RuntimeConfig {
            app_token: app_token.to_owned(),
            bot_token: bot_token.to_owned(),
            allowed_user_ids,
            configured_channel_id,
            api_base,
            max_message_bytes,
        })
    }
}

fn secret_value<'a>(
    secrets: &'a BTreeMap<String, tau_proto::SecretValue>,
    secret_name: &str,
    token_kind: &str,
) -> Result<&'a str, String> {
    secrets
        .get(secret_name)
        .map(tau_proto::SecretValue::expose_secret)
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| {
            format!("slack {token_kind} token secret `{secret_name}` is missing or empty")
        })
}

/// Slack text event details consumed by routing logic.
#[derive(Clone, Debug, Eq, PartialEq)]
struct SlackMessage {
    /// Slack event id used for duplicate suppression when available.
    event_id: Option<String>,
    /// Slack conversation id the event arrived in.
    channel_id: String,
    /// Slack channel type, such as `im` for DMs.
    channel_type: Option<String>,
    /// Sending Slack user id.
    user_id: String,
    /// Plain text delivered by Slack.
    text: String,
    /// Event type, normally `app_mention` or `message`.
    event_type: String,
    /// Optional Slack message subtype.
    subtype: Option<String>,
    /// Bot id when Slack identifies the sender as a bot.
    bot_id: Option<String>,
    /// Slack message timestamp used as a duplicate key fallback.
    ts: Option<String>,
}

/// Private DM learned through `start` when no fixed `channel_id` is configured.
#[derive(Clone)]
struct LinkedConversation {
    /// Slack DM channel id used as the reply destination.
    channel_id: String,
    /// Allowlisted Slack user id that established this DM link.
    user_id: String,
}

#[derive(Default)]
struct DuplicateCache {
    seen: HashSet<String>,
    order: VecDeque<String>,
}

impl DuplicateCache {
    /// Insert a duplicate key and return whether it was newly observed.
    fn insert_new(&mut self, key: String) -> bool {
        if self.seen.contains(&key) {
            return false;
        }
        self.seen.insert(key.clone());
        self.order.push_back(key);
        while self.order.len() > DUPLICATE_CACHE_SIZE {
            if let Some(old) = self.order.pop_front() {
                self.seen.remove(&old);
            }
        }
        true
    }
}

#[derive(Default)]
struct State {
    config: Option<RuntimeConfig>,
    registered_agents: HashSet<AgentId>,
    agent_labels: HashMap<AgentId, String>,
    selected_agent_by_channel: HashMap<String, AgentId>,
    learned_dm: Option<LinkedConversation>,
    worker_started: bool,
    worker_online: bool,
    worker_startup_failure_reported: bool,
    bot_user_id: Option<String>,
    duplicate_events: DuplicateCache,
}

struct Extension {
    state: Arc<Mutex<State>>,
    client: Arc<dyn SlackClient>,
    tx: mpsc::Sender<HarnessInputMessage>,
    shutdown: Arc<AtomicBool>,
}

impl Extension {
    /// Create a Slack extension instance using a supplied client
    /// implementation.
    fn new(client: Arc<dyn SlackClient>, tx: mpsc::Sender<HarnessInputMessage>) -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            client,
            tx,
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Apply validated configuration before the Socket Mode worker starts.
    fn apply_config(&self, cfg: RuntimeConfig) -> Result<(), String> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.worker_started {
            return Err(immutable_config_error());
        }
        let old_active = active_channel_locked(&state);
        let learned_dm_is_stale = cfg.configured_channel_id.is_some()
            || state
                .learned_dm
                .as_ref()
                .is_some_and(|link| !cfg.allowed_user_ids.contains(&link.user_id));
        if learned_dm_is_stale {
            state.learned_dm = None;
        }
        state.config = Some(cfg);
        let new_active = active_channel_locked(&state);
        if old_active != new_active {
            state.registered_agents.clear();
            state.selected_agent_by_channel.clear();
        }
        Ok(())
    }

    /// Clear inactive configuration and runtime routing state after a config
    /// error.
    fn clear_config_after_error(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.worker_started {
            return;
        }
        state.config = None;
        state.registered_agents.clear();
        state.selected_agent_by_channel.clear();
        state.learned_dm = None;
        state.bot_user_id = None;
        state.duplicate_events = DuplicateCache::default();
    }

    /// Report whether the Socket Mode worker has already started.
    fn worker_started(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .worker_started
    }

    /// Dispatch a Tau tool invocation owned by this extension.
    fn dispatch_tool(&self, invoke: ToolStarted) {
        let _ = self.tx.send(HarnessInputMessage::emit(Event::ToolProgress(
            ToolProgress {
                call_id: invoke.call_id.clone(),
                tool_name: invoke.tool_name.clone(),
                message: Some("slack tool started".to_owned()),
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
            _ => tool_error(invoke, "unknown slack tool".to_owned()),
        };
        let _ = self.tx.send(HarnessInputMessage::emit(event));
    }

    fn handle_register(&self, invoke: ToolStarted) -> Event {
        if let Err(message) = validate_object_fields(&invoke.arguments, &["enabled"]) {
            return tool_error(invoke, message);
        }
        let enabled = match cbor_bool_field(&invoke.arguments, "enabled") {
            Ok(enabled) => enabled,
            Err(message) => return tool_error(invoke, message),
        };
        if enabled {
            let startup = {
                let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if state.worker_started {
                    None
                } else {
                    let Some(cfg) = state.config.clone() else {
                        return tool_error(invoke, "slack extension is not configured".to_owned());
                    };
                    drop(state);
                    match self.prepare_worker_start(&cfg) {
                        Ok(startup) => Some((cfg, startup)),
                        Err(message) => return tool_error(invoke, message),
                    }
                }
            };
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.registered_agents.insert(invoke.agent_id.clone());
            state
                .agent_labels
                .entry(invoke.agent_id.clone())
                .or_insert_with(|| invoke.agent_id.to_string());
            if let Some((cfg, startup)) = startup {
                self.start_worker_locked(&mut state, cfg, Some(startup));
            }
        } else {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.registered_agents.remove(&invoke.agent_id);
            state
                .selected_agent_by_channel
                .retain(|_, agent| agent != &invoke.agent_id);
        }
        tool_result(
            invoke,
            if enabled {
                "registered for Slack messages; Socket Mode connection is starting"
            } else {
                "unregistered from Slack messages"
            },
        )
    }

    fn prepare_worker_start(&self, cfg: &RuntimeConfig) -> Result<WorkerStartup, String> {
        let bot_user_id = self.client.auth_test(cfg)?;
        let bot_user_id = validate_slack_id("auth.test user_id", &bot_user_id)?;
        let socket_url = self.client.open_socket(cfg)?;
        validate_socket_url(&socket_url)?;
        Ok(WorkerStartup {
            bot_user_id,
            socket_url,
        })
    }

    fn start_worker_locked(
        &self,
        state: &mut State,
        cfg: RuntimeConfig,
        startup: Option<WorkerStartup>,
    ) {
        if state.worker_started {
            return;
        }
        if let Some(startup) = &startup {
            state.bot_user_id = Some(startup.bot_user_id.clone());
        }
        state.worker_started = true;
        state.worker_startup_failure_reported = false;
        let state_arc = Arc::clone(&self.state);
        let tx = self.tx.clone();
        let client = Arc::clone(&self.client);
        let shutdown = Arc::clone(&self.shutdown);
        std::thread::spawn(move || {
            socket_worker_loop(state_arc, client, tx, cfg, startup, shutdown)
        });
    }

    fn report_worker_startup_failure_once(&self, cfg: &RuntimeConfig, message: &str) {
        let should_report = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.worker_online || state.worker_startup_failure_reported {
                false
            } else {
                state.worker_startup_failure_reported = true;
                true
            }
        };
        if should_report {
            let message = format!(
                "Slack Socket Mode startup failed; check std-slack tokens, Socket Mode settings, and network access: {}",
                sanitize_diagnostic(message, cfg)
            );
            let _ = self.tx.send(HarnessInputMessage::emit(Event::HarnessNotice(
                HarnessNotice {
                    kind: tau_proto::notice_kind::EXTENSION_NOTICE.to_owned(),
                    message: bounded_text(&message, MAX_DIAGNOSTIC_BYTES),
                    level: NoticeLevel::Warning,
                    always_show: false,
                },
            )));
        }
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
        let (cfg, channel_id) = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if !state.registered_agents.contains(&invoke.agent_id) {
                return tool_error(
                    invoke,
                    "slack_send requires slack_register(enabled: true) first".to_owned(),
                );
            }
            let Some(cfg) = state.config.clone() else {
                return tool_error(invoke, "slack extension is not configured".to_owned());
            };
            if message.len() > cfg.max_message_bytes {
                return tool_error(
                    invoke,
                    "`message` exceeds slack max_message_bytes".to_owned(),
                );
            }
            let Some(channel_id) = cfg.configured_channel_id.clone().or_else(|| {
                state
                    .learned_dm
                    .as_ref()
                    .map(|link| link.channel_id.clone())
            }) else {
                return tool_error(
                    invoke,
                    "slack conversation is not linked; send start in an allowlisted DM or configure channel_id"
                        .to_owned(),
                );
            };
            (cfg, channel_id)
        };
        let text = format!("[{}] {message}", invoke.agent_id.as_ref());
        match self.client.post_message(&cfg, &channel_id, &text) {
            Ok(()) => tool_result(invoke, "sent Slack message"),
            Err(message) => tool_error(invoke, message),
        }
    }

    fn process_slack_message(&self, message: SlackMessage) {
        if message.bot_id.is_some() || message.subtype.is_some() {
            return;
        }
        let Some(cfg) = self.config_for_allowed_message(&message) else {
            return;
        };
        if self.is_self_message(&message) {
            return;
        }
        let duplicate_key = message.event_id.clone().or_else(|| {
            message
                .ts
                .as_ref()
                .map(|ts| format!("{}:{ts}", message.channel_id))
        });
        if let Some(key) = duplicate_key
            && !self
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .duplicate_events
                .insert_new(key)
        {
            return;
        }
        if !matches!(message.event_type.as_str(), "app_mention" | "message") {
            return;
        }
        let is_dm = message.channel_type.as_deref() == Some("im");
        if message.event_type == "message" && !is_dm {
            return;
        }
        let active_channel = self.active_channel();
        if self.rejects_inactive_conversation(&cfg, &message, active_channel.as_deref(), is_dm) {
            return;
        }
        let Some(mut text) = self.trimmed_message_text(&cfg, &message) else {
            return;
        };
        text = self.strip_bot_mention(&text);
        if text.is_empty() {
            self.reply(&cfg, &message.channel_id, help_text());
            return;
        }
        let (command, rest) = parse_command(&text);
        if self.rejects_unlinked_command(&cfg, &message, active_channel.as_deref(), command) {
            return;
        }
        if self.handle_command(&cfg, &message, is_dm, command, rest) {
            return;
        }
        self.route_plain_text(&cfg, &message, &text);
    }

    fn config_for_allowed_message(&self, message: &SlackMessage) -> Option<RuntimeConfig> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = state.config.clone()?;
        if cfg.allowed_user_ids.contains(&message.user_id) {
            Some(cfg)
        } else {
            tracing::warn!(target: LOG_TARGET, user_id = %message.user_id, "ignoring Slack message from unallowed user");
            None
        }
    }

    fn is_self_message(&self, message: &SlackMessage) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .bot_user_id
            .as_ref()
            .is_some_and(|bot_user_id| bot_user_id == &message.user_id)
    }

    fn active_channel(&self) -> Option<String> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        active_channel_locked(&state)
    }

    fn rejects_inactive_conversation(
        &self,
        cfg: &RuntimeConfig,
        message: &SlackMessage,
        active_channel: Option<&str>,
        is_dm: bool,
    ) -> bool {
        if let Some(configured_channel_id) = &cfg.configured_channel_id {
            if message.channel_id != *configured_channel_id {
                self.reply(
                    cfg,
                    &message.channel_id,
                    "This Tau bridge is configured for a different Slack conversation.",
                );
                return true;
            }
        } else if active_channel.is_some_and(|channel_id| channel_id != message.channel_id) {
            self.reply(
                cfg,
                &message.channel_id,
                "This Tau bridge is already linked to a different Slack DM.",
            );
            return true;
        }

        if !is_dm && cfg.configured_channel_id.as_deref() != Some(message.channel_id.as_str()) {
            self.reply(
                cfg,
                &message.channel_id,
                "Slack channels are supported only when this channel_id is explicitly configured. Use an allowlisted DM and start to link DM mode.",
            );
            return true;
        }
        false
    }

    fn trimmed_message_text(&self, cfg: &RuntimeConfig, message: &SlackMessage) -> Option<String> {
        let text = message.text.trim();
        if text.is_empty() {
            self.reply(
                cfg,
                &message.channel_id,
                "Only text messages are supported by this Tau bridge.",
            );
            None
        } else if text.len() > cfg.max_message_bytes {
            self.reply(
                cfg,
                &message.channel_id,
                "Slack message is too large for this Tau bridge.",
            );
            None
        } else {
            Some(text.to_owned())
        }
    }

    fn strip_bot_mention(&self, text: &str) -> String {
        let bot_user_id = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .bot_user_id
            .clone();
        let Some(bot_user_id) = bot_user_id else {
            return text.trim().to_owned();
        };
        let mention = format!("<@{bot_user_id}>");
        text.trim()
            .strip_prefix(&mention)
            .map(str::trim)
            .unwrap_or_else(|| text.trim())
            .to_owned()
    }

    fn rejects_unlinked_command(
        &self,
        cfg: &RuntimeConfig,
        message: &SlackMessage,
        active_channel: Option<&str>,
        command: Option<&str>,
    ) -> bool {
        if cfg.configured_channel_id.is_some()
            || active_channel.is_some()
            || matches!(command, Some("start") | Some("/start"))
        {
            return false;
        }
        self.reply(
            cfg,
            &message.channel_id,
            "Send start in an allowlisted Slack DM before routing messages to Tau.",
        );
        true
    }

    fn handle_command(
        &self,
        cfg: &RuntimeConfig,
        message: &SlackMessage,
        is_dm: bool,
        command: Option<&str>,
        rest: &str,
    ) -> bool {
        match command {
            Some("start" | "/start") => {
                self.handle_start_command(cfg, message, is_dm);
                true
            }
            Some("agents" | "/agents") => {
                self.handle_agents_command(cfg, &message.channel_id);
                true
            }
            Some("select" | "/select") => {
                self.handle_select_command(cfg, &message.channel_id, rest);
                true
            }
            Some("to" | "/to") => {
                self.handle_to_command(cfg, message, rest);
                true
            }
            Some(command) if command.starts_with('/') => {
                self.reply(
                    cfg,
                    &message.channel_id,
                    "Unknown Slack command. Supported commands: start, agents, select, to.",
                );
                true
            }
            Some(_) | None => false,
        }
    }

    fn handle_start_command(&self, cfg: &RuntimeConfig, message: &SlackMessage, is_dm: bool) {
        if cfg.configured_channel_id.is_none() {
            if !is_dm {
                self.reply(
                    cfg,
                    &message.channel_id,
                    "Slack channel messages require an explicit configured channel_id.",
                );
                return;
            }
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.learned_dm = Some(LinkedConversation {
                channel_id: message.channel_id.clone(),
                user_id: message.user_id.clone(),
            });
        }
        self.reply(cfg, &message.channel_id, help_text());
    }

    fn handle_agents_command(&self, cfg: &RuntimeConfig, channel_id: &str) {
        let reply = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            agents_text(&state)
        };
        self.reply(cfg, channel_id, &reply);
    }

    fn handle_select_command(&self, cfg: &RuntimeConfig, channel_id: &str, rest: &str) {
        if rest.trim().is_empty() {
            self.reply(cfg, channel_id, "Usage: select <agent-id-or-prefix>");
            return;
        }
        let reply = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            match resolve_agent(&state, rest.trim()) {
                Ok(agent_id) => {
                    state
                        .selected_agent_by_channel
                        .insert(channel_id.to_owned(), agent_id.clone());
                    format!("Selected {}", agent_designator(&state, &agent_id))
                }
                Err(reply) => reply,
            }
        };
        self.reply(cfg, channel_id, &reply);
    }

    fn handle_to_command(&self, cfg: &RuntimeConfig, message: &SlackMessage, rest: &str) {
        let (target, body) = split_first(rest);
        if target.is_empty() || body.trim().is_empty() {
            self.reply(
                cfg,
                &message.channel_id,
                "Usage: to <agent-id-or-prefix> <message>",
            );
            return;
        }
        match self.resolve_registered_agent(target) {
            Ok(agent_id) => self.route_text(message, agent_id, body.trim()),
            Err(reply) => self.reply(cfg, &message.channel_id, &reply),
        }
    }

    fn route_plain_text(&self, cfg: &RuntimeConfig, message: &SlackMessage, text: &str) {
        match self.plain_text_target(&message.channel_id) {
            Ok(agent_id) => self.route_text(message, agent_id, text),
            Err(reply) => self.reply(cfg, &message.channel_id, &reply),
        }
    }

    fn plain_text_target(&self, channel_id: &str) -> Result<AgentId, String> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(agent_id) = state.selected_agent_by_channel.get(channel_id)
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
            Err(
                "No Tau agents are registered. Ask an agent to call slack_register(enabled: true)."
                    .to_owned(),
            )
        } else {
            Err(
                "Multiple Tau agents are registered. Use agents then select <agent-id-or-prefix>."
                    .to_owned(),
            )
        }
    }

    fn resolve_registered_agent(&self, target: &str) -> Result<AgentId, String> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        resolve_agent(&state, target)
    }

    fn reply(&self, cfg: &RuntimeConfig, channel_id: &str, text: &str) {
        let _ = self.client.post_message(cfg, channel_id, text);
    }

    fn route_text(&self, message: &SlackMessage, agent_id: AgentId, text: &str) {
        let source = sanitize_source_label(&message.user_id);
        let prompt = format!("[slack from {source}] {text}");
        let _ = self
            .tx
            .send(HarnessInputMessage::emit(Event::ExtPromptSubmitRequest(
                ExtPromptSubmitRequest {
                    agent_id,
                    text: prompt,
                    ctx_id: None,
                },
            )));
    }
}

fn active_channel_locked(state: &State) -> Option<String> {
    state
        .config
        .as_ref()
        .and_then(|cfg| cfg.configured_channel_id.clone())
        .or_else(|| {
            state
                .learned_dm
                .as_ref()
                .map(|link| link.channel_id.clone())
        })
}

impl Drop for Extension {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

fn socket_worker_loop(
    state: Arc<Mutex<State>>,
    client: Arc<dyn SlackClient>,
    tx: mpsc::Sender<HarnessInputMessage>,
    cfg: RuntimeConfig,
    startup: Option<WorkerStartup>,
    shutdown: Arc<AtomicBool>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::warn!(target: LOG_TARGET, error = %error, "failed to create Slack worker runtime");
            return;
        }
    };
    let ext = Extension {
        state,
        client,
        tx,
        shutdown: Arc::clone(&shutdown),
    };
    let mut backoff = INITIAL_RECONNECT_BACKOFF;
    let mut startup = startup;
    while !shutdown.load(Ordering::Relaxed) {
        match runtime.block_on(socket_worker_once(&ext, &cfg, startup.take())) {
            Ok(WorkerOutcome::ReconnectNow) => {
                backoff = INITIAL_RECONNECT_BACKOFF;
            }
            Ok(WorkerOutcome::Shutdown) => break,
            Err(message) => {
                ext.report_worker_startup_failure_once(&cfg, &message);
                tracing::warn!(target: LOG_TARGET, error = %message, "Slack Socket Mode worker failed");
                sleep_interruptibly(&shutdown, backoff);
                backoff = (backoff * 2).min(MAX_RECONNECT_BACKOFF);
            }
        }
    }
    let mut state = ext.state.lock().unwrap_or_else(|e| e.into_inner());
    state.worker_online = false;
}

struct WorkerStartup {
    bot_user_id: String,
    socket_url: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkerOutcome {
    ReconnectNow,
    Shutdown,
}

async fn socket_worker_once(
    ext: &Extension,
    cfg: &RuntimeConfig,
    startup: Option<WorkerStartup>,
) -> Result<WorkerOutcome, String> {
    let ws_url = match startup {
        Some(startup) => {
            let mut state = ext.state.lock().unwrap_or_else(|e| e.into_inner());
            state.bot_user_id = Some(startup.bot_user_id);
            startup.socket_url
        }
        None => {
            let bot_user_id = ext.client.auth_test(cfg)?;
            let bot_user_id = validate_slack_id("auth.test user_id", &bot_user_id)?;
            let ws_url = ext.client.open_socket(cfg)?;
            validate_socket_url(&ws_url)?;
            let mut state = ext.state.lock().unwrap_or_else(|e| e.into_inner());
            state.bot_user_id = Some(bot_user_id);
            ws_url
        }
    };
    let (mut ws, _response) = tokio_tungstenite::connect_async(&ws_url)
        .await
        .map_err(|error| {
            sanitize_socket_diagnostic(
                &format!("Slack websocket connect failed: {error}"),
                cfg,
                &ws_url,
            )
        })?;
    loop {
        if ext.shutdown.load(Ordering::Relaxed) {
            let _ = ws.close(None).await;
            return Ok(WorkerOutcome::Shutdown);
        }
        let next = tokio::time::timeout(Duration::from_millis(250), ws.next()).await;
        let Some(frame) = (match next {
            Ok(frame) => frame,
            Err(_) => continue,
        }) else {
            return Ok(WorkerOutcome::ReconnectNow);
        };
        let frame = frame.map_err(|error| {
            sanitize_diagnostic(&format!("Slack websocket frame failed: {error}"), cfg)
        })?;
        if let Some(outcome) = handle_socket_frame(ext, cfg, &mut ws, frame).await? {
            return Ok(outcome);
        }
    }
}

async fn handle_socket_frame(
    ext: &Extension,
    cfg: &RuntimeConfig,
    ws: &mut WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    frame: Message,
) -> Result<Option<WorkerOutcome>, String> {
    match frame {
        Message::Text(text) => handle_socket_text_frame(ext, cfg, ws, text.as_str()).await,
        Message::Close(_) => Ok(Some(WorkerOutcome::ReconnectNow)),
        Message::Ping(payload) => {
            ws.send(Message::Pong(payload)).await.map_err(|error| {
                sanitize_diagnostic(&format!("Slack websocket pong failed: {error}"), cfg)
            })?;
            Ok(None)
        }
        Message::Pong(_) | Message::Binary(_) | Message::Frame(_) => Ok(None),
    }
}

async fn handle_socket_text_frame(
    ext: &Extension,
    cfg: &RuntimeConfig,
    ws: &mut WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    text: &str,
) -> Result<Option<WorkerOutcome>, String> {
    if text.len() > MAX_SOCKET_FRAME_BYTES {
        tracing::warn!(target: LOG_TARGET, "dropping oversized Slack Socket Mode frame");
        return Ok(None);
    }
    let action = handle_socket_text(ext, text);
    if let Some(envelope_id) = &action.ack_envelope_id {
        send_socket_ack(cfg, ws, envelope_id).await?;
    }
    let outcome = action.outcome();
    if let Some(message) = action.message {
        ext.process_slack_message(message);
    }
    Ok(outcome)
}

async fn send_socket_ack(
    cfg: &RuntimeConfig,
    ws: &mut WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    envelope_id: &str,
) -> Result<(), String> {
    let ack = serde_json::json!({ "envelope_id": envelope_id }).to_string();
    ws.send(Message::Text(ack.into()))
        .await
        .map_err(|error| sanitize_diagnostic(&format!("Slack websocket ack failed: {error}"), cfg))
}

#[derive(Default)]
struct SocketAction {
    ack_envelope_id: Option<String>,
    message: Option<SlackMessage>,
    reconnect: bool,
    shutdown: bool,
}

impl SocketAction {
    fn outcome(&self) -> Option<WorkerOutcome> {
        if self.reconnect {
            Some(WorkerOutcome::ReconnectNow)
        } else if self.shutdown {
            Some(WorkerOutcome::Shutdown)
        } else {
            None
        }
    }
}

fn handle_socket_text(ext: &Extension, text: &str) -> SocketAction {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        tracing::warn!(target: LOG_TARGET, "dropping invalid Slack Socket Mode JSON");
        return SocketAction::default();
    };
    let ack_envelope_id = value
        .get("envelope_id")
        .and_then(|value| value.as_str())
        .filter(|id| !id.is_empty())
        .map(str::to_owned);
    let frame_type = value.get("type").and_then(|value| value.as_str());
    let mut action = SocketAction {
        ack_envelope_id,
        ..Default::default()
    };
    match frame_type {
        Some("hello") => {
            let mut state = ext.state.lock().unwrap_or_else(|e| e.into_inner());
            state.worker_online = true;
        }
        Some("disconnect") => {
            let reason = value.get("reason").and_then(|value| value.as_str());
            action.reconnect =
                matches!(reason, Some("warning" | "refresh_requested")) || reason.is_none();
            action.shutdown = !action.reconnect;
        }
        Some("events_api") => {
            action.message = decode_socket_event(&value);
        }
        _ => {}
    }
    action
}

fn decode_socket_event(value: &serde_json::Value) -> Option<SlackMessage> {
    let payload = value.get("payload")?;
    if payload.get("type").and_then(|value| value.as_str()) != Some("event_callback") {
        return None;
    }
    let event = payload.get("event")?;
    let event_type = event.get("type")?.as_str()?.to_owned();
    if !matches!(event_type.as_str(), "app_mention" | "message") {
        return None;
    }
    let text = event.get("text")?.as_str()?.to_owned();
    Some(SlackMessage {
        event_id: payload
            .get("event_id")
            .and_then(|value| value.as_str())
            .map(str::to_owned),
        channel_id: event.get("channel")?.as_str()?.to_owned(),
        channel_type: event
            .get("channel_type")
            .and_then(|value| value.as_str())
            .map(str::to_owned),
        user_id: event.get("user")?.as_str()?.to_owned(),
        text,
        event_type,
        subtype: event
            .get("subtype")
            .and_then(|value| value.as_str())
            .map(str::to_owned),
        bot_id: event
            .get("bot_id")
            .and_then(|value| value.as_str())
            .map(str::to_owned),
        ts: event
            .get("ts")
            .and_then(|value| value.as_str())
            .map(str::to_owned),
    })
}

fn run_with_client<R, W>(
    reader: R,
    writer: W,
    client: Arc<dyn SlackClient>,
) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write + Send + 'static,
{
    let mut reader = PeerInputReader::new(BufReader::new(reader));
    let mut writer = PeerOutputWriter::new(BufWriter::new(writer));
    tau_extension::Handshake::tool("tau-ext-slack")
        .subscribe([
            tau_proto::EventName::TOOL_STARTED,
            tau_proto::EventName::AGENT_DISPLAY_NAME_SET,
            tau_proto::EventName::AGENT_STARTED,
            tau_proto::EventName::SESSION_AGENT_UNLOADED,
            tau_proto::EventName::SESSION_SHUTDOWN,
        ])
        .register_tool_with_group_and_prompt_fragment(
            register_tool_spec(),
            Some(slack_tool_group()),
            None,
        )
        .register_tool_with_group_and_prompt_fragment(
            send_tool_spec(),
            Some(slack_tool_group()),
            None,
        )
        .ready_message("slack ready")
        .run(&mut writer)?;

    let (tx, rx) = mpsc::channel::<HarnessInputMessage>();
    let ext = Extension::new(client, tx.clone());
    let writer_shutdown = Arc::new(AtomicBool::new(false));
    let writer_shutdown_thread = Arc::clone(&writer_shutdown);
    let writer_handle = std::thread::spawn(move || -> Result<(), Box<dyn Error + Send>> {
        loop {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(msg) => {
                    writer
                        .write_message(&msg)
                        .map_err(|e| -> Box<dyn Error + Send> { Box::new(e) })?;
                    writer
                        .flush()
                        .map_err(|e| -> Box<dyn Error + Send> { Box::new(e) })?;
                }
                Err(mpsc::RecvTimeoutError::Timeout)
                    if writer_shutdown_thread.load(Ordering::Relaxed) =>
                {
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        Ok(())
    });

    while let Some(message) = reader.read_message()? {
        match message {
            HarnessOutputMessage::Configure(msg) => {
                if ext.worker_started() {
                    let _ = tx.send(HarnessInputMessage::ConfigError(ConfigError {
                        message: immutable_config_error(),
                    }));
                    continue;
                }
                match tau_extension::parse_config::<ExtConfig>(&msg.config)
                    .and_then(|cfg| cfg.validate(&msg.secrets))
                    .and_then(|cfg| ext.apply_config(cfg))
                {
                    Ok(()) => {}
                    Err(message) => {
                        ext.clear_config_after_error();
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
                        let mut state = ext.state.lock().unwrap_or_else(|e| e.into_inner());
                        state.registered_agents.remove(&unloaded.agent_id);
                        state.agent_labels.remove(&unloaded.agent_id);
                        state
                            .selected_agent_by_channel
                            .retain(|_, agent_id| agent_id != &unloaded.agent_id);
                    }
                    Event::SessionShutdown(_) => {
                        let mut state = ext.state.lock().unwrap_or_else(|e| e.into_inner());
                        state.registered_agents.clear();
                        state.agent_labels.clear();
                        state.selected_agent_by_channel.clear();
                    }
                    _ => {}
                }
            }
            HarnessOutputMessage::Disconnect(_) => {
                ext.shutdown.store(true, Ordering::Relaxed);
                writer_shutdown.store(true, Ordering::Relaxed);
                break;
            }
            _ => {}
        }
    }
    ext.shutdown.store(true, Ordering::Relaxed);
    writer_shutdown.store(true, Ordering::Relaxed);
    drop(ext);
    drop(tx);
    writer_handle
        .join()
        .map_err(|e| -> Box<dyn Error> { format!("writer thread panicked: {e:?}").into() })?
        .map_err(|e| -> Box<dyn Error> { e })?;
    Ok(())
}

fn immutable_config_error() -> String {
    "slack configuration cannot be changed after Socket Mode has started; restart Tau to apply new Slack settings"
        .to_owned()
}

fn slack_tool_group() -> tau_proto::ToolGroup {
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
            "Register or unregister this agent for Slack messages. Use enabled=true to allow an allowlisted Slack user to send prompts to this agent; use enabled=false to stop listening. When replying to Slack-originated prompts, use slack_send."
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
            title: Some("Register for Slack".to_owned()),
            arguments: CborValue::Map(vec![example_field("enabled", CborValue::Bool(true))]),
            note: Some("Use enabled=false to stop receiving Slack prompts.".to_owned()),
            subcommand: None,
        }],
    }
}

fn send_tool_spec() -> ToolSpec {
    ToolSpec {
        name: tau_proto::ToolName::new(SEND_TOOL_NAME),
        model_visible_name: Some(tau_proto::ToolName::new(SEND_TOOL_NAME)),
        description: Some(
            "Send a text message to the configured or linked Slack conversation. Only registered agents may use this tool; it cannot choose arbitrary channel, user, or thread destinations. Use it to answer prompts prefixed with [slack from ...]."
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
            title: Some("Send a Slack reply".to_owned()),
            arguments: CborValue::Map(vec![example_field(
                "message",
                example_text("Thanks, I’ll look into it."),
            )]),
            note: Some("There is no channel_id argument; the configured or linked conversation is used.".to_owned()),
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
    let (token, rest) = split_first(text);
    match token {
        "start" | "/start" | "agents" | "/agents" | "select" | "/select" | "to" | "/to" => {
            (Some(token), rest)
        }
        token if token.starts_with('/') => (Some(token), rest),
        _ => (None, ""),
    }
}

fn help_text() -> &'static str {
    "Tau Slack bridge linked. Commands: agents, select <agent-id-or-prefix>, to <agent-id-or-prefix> <message>. Plain text goes to the selected agent, or to the only registered agent."
}

fn validate_slack_id(field: &str, value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("slack `{field}` must not contain empty ids"));
    }
    if value.len() > 64
        || !value
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit())
    {
        return Err(format!("slack `{field}` contains an invalid Slack id"));
    }
    Ok(value.to_owned())
}

fn sanitize_source_label(user_id: &str) -> String {
    user_id
        .chars()
        .filter(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit())
        .take(64)
        .collect::<String>()
}

fn validate_api_base(api_base: &str) -> Result<(), String> {
    if api_base.is_empty() {
        return Err("slack `api_base` must not be empty".to_owned());
    }
    let url = url::Url::parse(api_base)
        .map_err(|e| format!("slack `api_base` must be a valid URL: {e}"))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err("slack `api_base` must not include userinfo".to_owned());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("slack `api_base` must not include query or fragment".to_owned());
    }
    match url.scheme() {
        "https" => Ok(()),
        "http" if url.host().is_some_and(is_loopback_host) => Ok(()),
        "http" => Err("slack `api_base` may use http only for loopback hosts".to_owned()),
        _ => Err("slack `api_base` must use https, or http for loopback tests".to_owned()),
    }
}

fn validate_socket_url(ws_url: &str) -> Result<(), String> {
    let url =
        url::Url::parse(ws_url).map_err(|e| format!("Slack Socket Mode URL is invalid: {e}"))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err("Slack Socket Mode URL must not include userinfo".to_owned());
    }
    match url.scheme() {
        "wss" => Ok(()),
        "ws" if url.host().is_some_and(is_loopback_host) => Ok(()),
        "ws" => Err("Slack Socket Mode URL may use ws only for loopback hosts".to_owned()),
        _ => Err("Slack Socket Mode URL must use wss, or ws for loopback tests".to_owned()),
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

struct HttpSlackClient {
    agent: ureq::Agent,
}

impl HttpSlackClient {
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

    fn post(
        &self,
        cfg: &RuntimeConfig,
        method: &str,
        token: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let url = format!("{}/{method}", cfg.api_base);
        let mut response = self
            .agent
            .post(&url)
            .header("Authorization", &format!("Bearer {token}"))
            .content_type("application/json")
            .send(body.to_string())
            .map_err(|error| {
                sanitize_diagnostic(&format!("Slack transport error: {error}"), cfg)
            })?;
        let status = response.status();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|e| sanitize_diagnostic(&format!("reading Slack response: {e}"), cfg))?;
        parse_slack_api_response(cfg, method, status.as_u16(), retry_after.as_deref(), &text)
    }
}

impl Default for HttpSlackClient {
    fn default() -> Self {
        Self {
            agent: Self::agent(),
        }
    }
}

fn parse_slack_api_response(
    cfg: &RuntimeConfig,
    method: &str,
    status_code: u16,
    retry_after: Option<&str>,
    text: &str,
) -> Result<serde_json::Value, String> {
    if status_code == 429
        && method == "chat.postMessage"
        && let Some(retry_after) = retry_after
    {
        return Err(format!(
            "Slack rate limited chat.postMessage; retry after {retry_after}s"
        ));
    }
    if !(200..300).contains(&status_code) {
        return Err(format!(
            "Slack returned HTTP {status_code}: {}",
            sanitize_diagnostic(text, cfg)
        ));
    }
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("invalid Slack JSON response: {e}"))?;
    if value.get("ok").and_then(|value| value.as_bool()) != Some(true) {
        let error = value
            .get("error")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown_error");
        return Err(format!(
            "Slack API {method} failed: {}",
            sanitize_diagnostic(error, cfg)
        ));
    }
    Ok(value)
}

impl SlackClient for HttpSlackClient {
    fn open_socket(&self, cfg: &RuntimeConfig) -> Result<String, String> {
        let value = self.post(
            cfg,
            "apps.connections.open",
            &cfg.app_token,
            serde_json::json!({}),
        )?;
        value
            .get("url")
            .and_then(|value| value.as_str())
            .map(str::to_owned)
            .ok_or_else(|| "Slack apps.connections.open response missing url".to_owned())
    }

    fn auth_test(&self, cfg: &RuntimeConfig) -> Result<String, String> {
        let value = self.post(cfg, "auth.test", &cfg.bot_token, serde_json::json!({}))?;
        value
            .get("user_id")
            .and_then(|value| value.as_str())
            .map(str::to_owned)
            .ok_or_else(|| "Slack auth.test response missing user_id".to_owned())
    }

    fn post_message(
        &self,
        cfg: &RuntimeConfig,
        channel_id: &str,
        text: &str,
    ) -> Result<(), String> {
        self.post(
            cfg,
            "chat.postMessage",
            &cfg.bot_token,
            serde_json::json!({ "channel": channel_id, "text": text }),
        )?;
        Ok(())
    }
}

fn sanitize_diagnostic(text: &str, cfg: &RuntimeConfig) -> String {
    let text = text
        .replace(&cfg.app_token, "<redacted>")
        .replace(&cfg.bot_token, "<redacted>");
    bounded_text(&text, MAX_DIAGNOSTIC_BYTES)
}

fn sanitize_socket_diagnostic(text: &str, cfg: &RuntimeConfig, socket_url: &str) -> String {
    let text = text
        .replace(socket_url, "<redacted-socket-url>")
        .replace(&cfg.app_token, "<redacted>")
        .replace(&cfg.bot_token, "<redacted>");
    bounded_text(&text, MAX_DIAGNOSTIC_BYTES)
}

fn bounded_text(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests;
