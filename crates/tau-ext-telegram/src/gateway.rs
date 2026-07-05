//! Standalone Telegram gateway daemon MVP.
//!
//! This module implements the single-owner gateway slice: stream locking,
//! durable update offsets, Telegram allowlist/chat policy, help/status command
//! handling, and private local socket support for gateway-side sidecar
//! heartbeat and registration-lease bookkeeping. Prompt routing and outbound
//! gateway sends are deliberately left for later stages.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{env, thread};

use crate::stream_owner::{
    UpdateStreamLock, telegram_contention_diagnostic, webhook_active_message,
};
use crate::{
    DEFAULT_API_BASE, DEFAULT_POLL_TIMEOUT_SECONDS, HttpTelegramClient, RuntimeConfig,
    TelegramClient, TgMessage, TgUpdate, is_private_message_chat, parse_command, validate_api_base,
};

/// Maximum Telegram reply text emitted by the gateway MVP.
const MAX_REPLY_BYTES: usize = 3500;

/// Number of update ids kept for restart duplicate suppression.
const RECENT_UPDATE_LIMIT: usize = 128;

/// Version of the local gateway status socket protocol.
const SOCKET_PROTOCOL_VERSION: u32 = 1;

/// Maximum bytes read from one local socket request.
const MAX_SOCKET_REQUEST_BYTES: usize = 8192;

/// Interval sidecars should use for gateway heartbeats.
const SIDECAR_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

/// Maximum time a registration remains live without a sidecar heartbeat.
const REGISTRATION_LEASE_DURATION: Duration = Duration::from_secs(30);

/// Default environment variable carrying the bot token.
const DEFAULT_BOT_TOKEN_ENV: &str = "TELEGRAM_BOT_TOKEN";

/// Outcome of processing one Telegram update/message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UpdateOutcome {
    /// The update was handled or intentionally ignored and its offset may be
    /// advanced.
    AdvanceOffset,
    /// A required side effect failed; do not advance this update's offset and
    /// stop processing later updates in the same batch.
    NeedsRedelivery,
}

impl UpdateOutcome {
    /// Convert a required Telegram reply result into an update outcome.
    fn from_required_reply(success: bool) -> Self {
        if success {
            Self::AdvanceOffset
        } else {
            Self::NeedsRedelivery
        }
    }
}

/// Run the gateway daemon using process command-line arguments and environment.
pub(super) fn run_from_env() -> Result<(), Box<dyn Error>> {
    let args = env::args_os().collect::<Vec<_>>();
    if args
        .iter()
        .skip(1)
        .any(|arg| arg == "--help" || arg == "-h")
    {
        println!("{}", gateway_usage());
        return Ok(());
    }
    let config = GatewayConfig::from_env_args(args, |name| env::var(name).ok())?;
    let gateway = Gateway::new(config, Arc::new(HttpTelegramClient::default()))?;
    gateway.run_forever()
}

/// Validated configuration for the standalone gateway daemon.
#[derive(Clone)]
struct GatewayConfig {
    /// Resolved Telegram bot token. Never log or serialize this value.
    bot_token: String,
    /// Telegram users allowed to interact with this gateway.
    allowed_user_ids: HashSet<i64>,
    /// Optional configured chat accepted for commands and replies.
    configured_chat_id: Option<i64>,
    /// Validated Telegram Bot API base URL.
    api_base: String,
    /// Long-poll timeout passed to `getUpdates`.
    poll_timeout_seconds: u64,
    /// Private state directory containing durable gateway state and stream
    /// locks.
    state_dir: PathBuf,
    /// Private runtime directory containing the gateway local socket.
    runtime_dir: PathBuf,
}

impl GatewayConfig {
    /// Parse command-line flags and resolve the bot token from the environment.
    fn from_env_args<I, F>(args: I, get_env: F) -> Result<Self, String>
    where
        I: IntoIterator<Item = OsString>,
        F: Fn(&str) -> Option<String>,
    {
        let mut parser = GatewayArgParser::new(args.into_iter().skip(1));
        let raw = parser.parse()?;
        let bot_token_env = raw
            .bot_token_env
            .as_deref()
            .unwrap_or(DEFAULT_BOT_TOKEN_ENV);
        let bot_token = get_env(bot_token_env)
            .map(|token| token.trim().to_owned())
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                format!(
                    "telegram gateway requires bot token in environment variable `{bot_token_env}`"
                )
            })?;
        if raw.allowed_user_ids.is_empty() {
            return Err("telegram gateway requires at least one --allowed-user-id".to_owned());
        }
        let api_base = raw
            .api_base
            .unwrap_or_else(|| DEFAULT_API_BASE.to_owned())
            .trim_end_matches('/')
            .to_owned();
        validate_api_base(&api_base)?;
        let state_dir = raw.state_dir.unwrap_or_else(|| default_state_dir(&get_env));
        let runtime_dir = raw
            .runtime_dir
            .unwrap_or_else(|| default_runtime_dir(&get_env, &state_dir));
        Ok(Self {
            bot_token,
            allowed_user_ids: raw.allowed_user_ids.into_iter().collect(),
            configured_chat_id: raw.configured_chat_id,
            api_base,
            poll_timeout_seconds: raw
                .poll_timeout_seconds
                .unwrap_or(DEFAULT_POLL_TIMEOUT_SECONDS),
            state_dir,
            runtime_dir,
        })
    }

    /// Convert gateway configuration into the shared Telegram runtime config.
    fn runtime_config(&self) -> RuntimeConfig {
        RuntimeConfig {
            bot_token: self.bot_token.clone(),
            allowed_user_ids: self.allowed_user_ids.clone(),
            configured_chat_id: self.configured_chat_id,
            api_base: self.api_base.clone(),
            poll_timeout_seconds: self.poll_timeout_seconds,
        }
    }
}

/// Raw command-line configuration before environment/default resolution.
#[derive(Default)]
struct RawGatewayArgs {
    /// Environment variable containing the Telegram bot token.
    bot_token_env: Option<String>,
    /// Telegram users allowed to use this bot.
    allowed_user_ids: Vec<i64>,
    /// Optional fixed Telegram chat id.
    configured_chat_id: Option<i64>,
    /// Optional Bot API base URL override.
    api_base: Option<String>,
    /// Optional Telegram long-poll timeout.
    poll_timeout_seconds: Option<u64>,
    /// Optional private state directory override.
    state_dir: Option<PathBuf>,
    /// Optional private runtime socket directory override.
    runtime_dir: Option<PathBuf>,
}

/// Minimal flag parser that avoids adding a CLI dependency for the MVP daemon.
struct GatewayArgParser<I> {
    /// Remaining command-line arguments.
    args: I,
}

impl<I> GatewayArgParser<I>
where
    I: Iterator<Item = OsString>,
{
    /// Create a parser over arguments after the binary name.
    fn new(args: I) -> Self {
        Self { args }
    }

    /// Parse all gateway flags into raw values.
    fn parse(&mut self) -> Result<RawGatewayArgs, String> {
        let mut raw = RawGatewayArgs::default();
        while let Some(arg) = self.args.next() {
            let arg = arg
                .into_string()
                .map_err(|_| "telegram gateway arguments must be valid UTF-8".to_owned())?;
            match arg.as_str() {
                "--bot-token-env" => raw.bot_token_env = Some(self.next_value(&arg)?),
                "--allowed-user-id" => {
                    raw.allowed_user_ids
                        .push(parse_i64_flag(&arg, &self.next_value(&arg)?)?);
                }
                "--allowed-user-ids" => {
                    for value in self.next_value(&arg)?.split(',') {
                        raw.allowed_user_ids.push(parse_i64_flag(&arg, value)?);
                    }
                }
                "--chat-id" => {
                    raw.configured_chat_id = Some(parse_i64_flag(&arg, &self.next_value(&arg)?)?);
                }
                "--api-base" => raw.api_base = Some(self.next_value(&arg)?),
                "--poll-timeout-seconds" => {
                    raw.poll_timeout_seconds = Some(parse_u64_flag(&arg, &self.next_value(&arg)?)?);
                }
                "--state-dir" => raw.state_dir = Some(PathBuf::from(self.next_value(&arg)?)),
                "--runtime-dir" => raw.runtime_dir = Some(PathBuf::from(self.next_value(&arg)?)),
                "--help" | "-h" => return Err(gateway_usage()),
                other => return Err(format!("unknown telegram gateway argument `{other}`")),
            }
        }
        Ok(raw)
    }

    /// Return the next argument as this flag's value.
    fn next_value(&mut self, flag: &str) -> Result<String, String> {
        self.args
            .next()
            .ok_or_else(|| format!("missing value for {flag}"))?
            .into_string()
            .map_err(|_| format!("{flag} value must be valid UTF-8"))
    }
}

/// Runtime gateway daemon with owned Telegram stream and local socket.
struct Gateway {
    /// Shared Telegram runtime configuration.
    cfg: RuntimeConfig,
    /// HTTP client used for Telegram Bot API calls.
    client: Arc<dyn TelegramClient>,
    /// Durable state file path scoped to the stream fingerprint.
    state_path: PathBuf,
    /// Mutable durable gateway state.
    durable: GatewayDurableState,
    /// Shared socket state served by the local socket.
    socket_state: Arc<GatewaySocketState>,
    /// Resources that keep production stream/socket ownership alive.
    _resources: GatewayResources,
}

/// Resources held for the lifetime of a gateway instance.
enum GatewayResources {
    /// Production resources that enforce singleton stream ownership and socket
    /// cleanup.
    Production {
        /// Held update-stream lock; dropping it releases stream ownership.
        _update_stream_lock: UpdateStreamLock,
        /// Local socket guard that removes the socket path on daemon exit.
        _socket_guard: GatewaySocketGuard,
    },
    /// Test mode avoids real locks and sockets while exercising gateway logic.
    #[cfg(test)]
    Test,
}

impl Gateway {
    /// Construct a gateway, acquire stream ownership, and bind its local
    /// socket.
    fn new(config: GatewayConfig, client: Arc<dyn TelegramClient>) -> Result<Self, String> {
        create_private_dir(&config.state_dir)?;
        create_private_dir(&config.runtime_dir)?;
        let cfg = config.runtime_config();
        let stream_hash = cfg.stream_identity().fingerprint();
        let update_stream_lock =
            UpdateStreamLock::acquire(&config.state_dir, cfg.stream_identity())?;
        let webhook = client.get_webhook_info(&cfg)?;
        if !webhook.url.trim().is_empty() {
            return Err(webhook_active_message(&webhook));
        }
        let state_path = config.state_dir.join(format!("{stream_hash}.json"));
        let mut durable = GatewayDurableState::load(&state_path, &stream_hash)?;
        if durable.reconcile_with_config(&cfg) {
            durable.save(&state_path)?;
        }
        let socket_path = config.runtime_dir.join(format!("{stream_hash}.sock"));
        let socket_state = Arc::new(GatewaySocketState::new(
            &cfg,
            &durable,
            stream_hash.clone(),
            socket_path.clone(),
        ));
        let socket_guard = GatewaySocketGuard::bind(socket_path, Arc::clone(&socket_state))?;
        Ok(Self {
            cfg,
            client,
            state_path,
            durable,
            socket_state,
            _resources: GatewayResources::Production {
                _update_stream_lock: update_stream_lock,
                _socket_guard: socket_guard,
            },
        })
    }

    /// Run Telegram long polling forever.
    fn run_forever(mut self) -> Result<(), Box<dyn Error>> {
        eprintln!(
            "Telegram gateway started; local socket: {}",
            self.socket_state.socket_path.display()
        );
        loop {
            match self
                .client
                .get_updates(&self.cfg, self.durable.next_update_offset)
            {
                Ok(updates) => self.process_updates(updates)?,
                Err(message) => {
                    if let Some(diagnostic) = telegram_contention_diagnostic(&message) {
                        return Err(diagnostic.into());
                    }
                    tracing::warn!(target: crate::LOG_TARGET, error = %message, "telegram gateway polling failed");
                    thread::sleep(Duration::from_secs(5));
                }
            }
        }
    }

    /// Process a batch of Telegram updates in order.
    fn process_updates(&mut self, updates: Vec<TgUpdate>) -> Result<(), String> {
        for update in updates {
            if self.process_update(update)? == UpdateOutcome::NeedsRedelivery {
                break;
            }
        }
        Ok(())
    }

    /// Process one update and durably advance the offset after handling.
    fn process_update(&mut self, update: TgUpdate) -> Result<UpdateOutcome, String> {
        if self.durable.has_recent_update(update.update_id) {
            self.advance_offset(update.update_id)?;
            return Ok(UpdateOutcome::AdvanceOffset);
        }
        if let Some(message) = update.message.as_ref()
            && self.process_message(message) == UpdateOutcome::NeedsRedelivery
        {
            return Ok(UpdateOutcome::NeedsRedelivery);
        }
        self.durable.remember_update(update.update_id);
        self.durable.processed_update_count = self.durable.processed_update_count.saturating_add(1);
        self.advance_offset(update.update_id)?;
        Ok(UpdateOutcome::AdvanceOffset)
    }

    /// Advance and persist the next update offset.
    fn advance_offset(&mut self, update_id: i64) -> Result<(), String> {
        let next = update_id.saturating_add(1);
        if self
            .durable
            .next_update_offset
            .is_none_or(|offset| offset < next)
        {
            self.durable.next_update_offset = Some(next);
        }
        self.persist()
    }

    /// Persist durable state and publish an updated local status snapshot.
    fn persist(&mut self) -> Result<(), String> {
        self.durable.save(&self.state_path)?;
        let stream_hash = self.durable.stream_hash.clone();
        self.socket_state
            .set_status(GatewayStatus::new(&self.cfg, &self.durable, stream_hash));
        Ok(())
    }

    /// Handle a Telegram message after update decoding.
    fn process_message(&mut self, message: &TgMessage) -> UpdateOutcome {
        if !self.cfg.allowed_user_ids.contains(&message.user_id) {
            tracing::warn!(
                target: crate::LOG_TARGET,
                user_id = message.user_id,
                "telegram gateway ignored message from unallowed user"
            );
            self.durable.rejected_update_count =
                self.durable.rejected_update_count.saturating_add(1);
            return UpdateOutcome::AdvanceOffset;
        }
        let text = message.text.as_deref().unwrap_or_default().trim();
        if text.is_empty() {
            return self.reply_if_chat_is_active(
                message,
                "Telegram gateway MVP accepts text commands only.",
            );
        }
        if let Some(outcome) = self.maybe_handle_start(message, text) {
            return outcome;
        }
        if !self.chat_is_active(message) {
            return self.reject_inactive_chat(message);
        }
        let (command, rest) = parse_command(text);
        UpdateOutcome::from_required_reply(match command {
            Some("/help") => self.reply(message.chat_id, gateway_help_text()),
            Some("/status") => self.reply(message.chat_id, &self.status_text()),
            Some("/start") => self.reply(message.chat_id, gateway_help_text()),
            Some(_) => self.reply(
                message.chat_id,
                "Unknown Telegram gateway command. Supported commands: /start, /help, /status.",
            ),
            None if rest.is_empty() => self.reply(message.chat_id, gateway_not_routing_text()),
            None => self.reply(message.chat_id, gateway_not_routing_text()),
        })
    }

    /// Handle `/start` chat-linking before the generic active-chat check.
    fn maybe_handle_start(&mut self, message: &TgMessage, text: &str) -> Option<UpdateOutcome> {
        let (command, _) = parse_command(text);
        if command != Some("/start") {
            return None;
        }
        if let Some(chat_id) = self.cfg.configured_chat_id {
            if message.chat_id == chat_id {
                return Some(UpdateOutcome::from_required_reply(
                    self.reply(message.chat_id, gateway_help_text()),
                ));
            }
            if is_private_message_chat(message) {
                return Some(UpdateOutcome::from_required_reply(self.reply(
                    message.chat_id,
                    "This Telegram gateway is configured for a different chat.",
                )));
            }
            tracing::warn!(
                target: crate::LOG_TARGET,
                chat_id = message.chat_id,
                "telegram gateway ignored start command from unconfigured group"
            );
            return Some(UpdateOutcome::AdvanceOffset);
        }
        if !is_private_message_chat(message) {
            tracing::warn!(
                target: crate::LOG_TARGET,
                chat_id = message.chat_id,
                "telegram gateway ignored unconfigured group start command"
            );
            return Some(UpdateOutcome::AdvanceOffset);
        }
        match self.durable.linked_chat {
            Some(link) if link.chat_id == message.chat_id && link.user_id == message.user_id => {
                Some(UpdateOutcome::from_required_reply(
                    self.reply(message.chat_id, gateway_help_text()),
                ))
            }
            Some(_) => Some(UpdateOutcome::from_required_reply(self.reply(
                message.chat_id,
                "This Telegram gateway is already linked to a different private chat.",
            ))),
            None => {
                if self.reply(message.chat_id, gateway_help_text()) {
                    self.durable.linked_chat = Some(GatewayLinkedChat {
                        chat_id: message.chat_id,
                        user_id: message.user_id,
                    });
                    Some(UpdateOutcome::AdvanceOffset)
                } else {
                    Some(UpdateOutcome::NeedsRedelivery)
                }
            }
        }
    }

    /// Return whether this message arrived in the configured or linked chat.
    fn chat_is_active(&self, message: &TgMessage) -> bool {
        match self.cfg.configured_chat_id {
            Some(chat_id) => message.chat_id == chat_id,
            None => self.durable.linked_chat.is_some_and(|link| {
                link.chat_id == message.chat_id && link.user_id == message.user_id
            }),
        }
    }

    /// Send an inactive-chat rejection without changing the link.
    fn reject_inactive_chat(&self, message: &TgMessage) -> UpdateOutcome {
        if self.cfg.configured_chat_id.is_some() {
            if is_private_message_chat(message) {
                return UpdateOutcome::from_required_reply(self.reply(
                    message.chat_id,
                    "This Telegram gateway is configured for a different chat.",
                ));
            }
            tracing::warn!(
                target: crate::LOG_TARGET,
                chat_id = message.chat_id,
                "telegram gateway ignored unconfigured group message"
            );
            UpdateOutcome::AdvanceOffset
        } else if !is_private_message_chat(message) {
            tracing::warn!(
                target: crate::LOG_TARGET,
                chat_id = message.chat_id,
                "telegram gateway ignored unconfigured group message"
            );
            UpdateOutcome::AdvanceOffset
        } else {
            UpdateOutcome::from_required_reply(self.reply(
                message.chat_id,
                "Telegram gateway is not linked to this chat. Send /start from one allowlisted private chat.",
            ))
        }
    }

    /// Reply only when a message already belongs to the active chat.
    fn reply_if_chat_is_active(&self, message: &TgMessage, text: &str) -> UpdateOutcome {
        if self.chat_is_active(message) {
            UpdateOutcome::from_required_reply(self.reply(message.chat_id, text))
        } else {
            UpdateOutcome::AdvanceOffset
        }
    }

    /// Send one bounded Telegram reply, logging failures without exposing
    /// tokens.
    fn reply(&self, chat_id: i64, text: &str) -> bool {
        let text = bounded_reply_text(text);
        match self.client.send_message(&self.cfg, chat_id, &text) {
            Ok(()) => true,
            Err(message) => {
                tracing::warn!(target: crate::LOG_TARGET, error = %message, "telegram gateway reply failed");
                false
            }
        }
    }

    /// Build a human-readable status message for `/status`.
    fn status_text(&self) -> String {
        let linked_chat = self
            .durable
            .linked_chat
            .map(|link| link.chat_id.to_string())
            .unwrap_or_else(|| "none".to_owned());
        format!(
            "Tau Telegram gateway status:\nstream: {}\nnext update offset: {}\nallowed users: {}\nconfigured chat: {}\nlinked chat: {}\nprocessed updates: {}\nrejected updates: {}\nrouting: help/status only",
            self.durable.stream_hash,
            self.durable
                .next_update_offset
                .map(|offset| offset.to_string())
                .unwrap_or_else(|| "none".to_owned()),
            self.cfg.allowed_user_ids.len(),
            self.cfg
                .configured_chat_id
                .map(|chat_id| chat_id.to_string())
                .unwrap_or_else(|| "none".to_owned()),
            linked_chat,
            self.durable.processed_update_count,
            self.durable.rejected_update_count,
        )
    }
}

/// Durable private-chat link learned with `/start`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
struct GatewayLinkedChat {
    /// Telegram private chat id.
    chat_id: i64,
    /// Allowlisted user that established the link.
    user_id: i64,
}

/// Durable gateway state scoped to one stream fingerprint.
#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
struct GatewayDurableState {
    /// Non-secret stream fingerprint this state belongs to.
    stream_hash: String,
    /// Next Telegram update offset to request.
    next_update_offset: Option<i64>,
    /// Private chat link, when no fixed chat is configured.
    linked_chat: Option<GatewayLinkedChat>,
    /// Recently handled update ids for duplicate suppression.
    recent_update_ids: Vec<i64>,
    /// Number of updates intentionally handled by this gateway state.
    processed_update_count: u64,
    /// Number of updates rejected before side effects due to allowlist policy.
    rejected_update_count: u64,
}

impl GatewayDurableState {
    /// Load state from disk, returning empty state when no file exists.
    fn load(path: &Path, stream_hash: &str) -> Result<Self, String> {
        match fs::read_to_string(path) {
            Ok(text) => {
                let mut state: Self = serde_json::from_str(&text)
                    .map_err(|error| format!("reading Telegram gateway state: {error}"))?;
                if state.stream_hash != stream_hash {
                    return Err("Telegram gateway state stream hash mismatch".to_owned());
                }
                state.recent_update_ids.truncate(RECENT_UPDATE_LIMIT);
                Ok(state)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                stream_hash: stream_hash.to_owned(),
                ..Self::default()
            }),
            Err(error) => Err(format!("reading Telegram gateway state: {error}")),
        }
    }

    /// Save state atomically with private file permissions.
    fn save(&self, path: &Path) -> Result<(), String> {
        let parent = path
            .parent()
            .ok_or_else(|| "Telegram gateway state path has no parent".to_owned())?;
        create_private_dir(parent)?;
        let tmp_path = path.with_extension("json.tmp");
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&tmp_path)
            .map_err(|error| format!("opening Telegram gateway state file: {error}"))?;
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|error| format!("encoding Telegram gateway state: {error}"))?;
        file.write_all(&bytes)
            .map_err(|error| format!("writing Telegram gateway state: {error}"))?;
        file.write_all(b"\n")
            .map_err(|error| format!("writing Telegram gateway state: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("syncing Telegram gateway state: {error}"))?;
        fs::rename(&tmp_path, path)
            .map_err(|error| format!("installing Telegram gateway state: {error}"))?;
        Ok(())
    }

    /// Clear durable chat state that is invalid under the current runtime
    /// configuration.
    fn reconcile_with_config(&mut self, cfg: &RuntimeConfig) -> bool {
        let Some(linked_chat) = self.linked_chat else {
            return false;
        };
        if cfg.configured_chat_id.is_some() || !cfg.allowed_user_ids.contains(&linked_chat.user_id)
        {
            self.linked_chat = None;
            return true;
        }
        false
    }

    /// Return whether an update id was recently processed.
    fn has_recent_update(&self, update_id: i64) -> bool {
        self.recent_update_ids.contains(&update_id)
    }

    /// Record a newly processed update id.
    fn remember_update(&mut self, update_id: i64) {
        if self.has_recent_update(update_id) {
            return;
        }
        self.recent_update_ids.push(update_id);
        let excess = self
            .recent_update_ids
            .len()
            .saturating_sub(RECENT_UPDATE_LIMIT);
        if excess > 0 {
            self.recent_update_ids.drain(0..excess);
        }
    }
}

/// JSON status snapshot served over the local gateway socket.
#[derive(Clone, Debug, serde::Serialize)]
struct GatewayStatus {
    /// Local socket protocol version.
    protocol_version: u32,
    /// Non-secret stream fingerprint.
    stream_hash: String,
    /// Number of configured allowlisted users.
    allowed_user_count: usize,
    /// Optional configured chat id.
    configured_chat_id: Option<i64>,
    /// Optional linked private chat id.
    linked_chat_id: Option<i64>,
    /// Next Telegram update offset.
    next_update_offset: Option<i64>,
    /// Recently remembered update-id count.
    recent_update_count: usize,
    /// Processed update count.
    processed_update_count: u64,
    /// Rejected update count.
    rejected_update_count: u64,
    /// Number of currently connected sidecars.
    active_sidecar_count: usize,
    /// Number of currently live agent registrations.
    active_registration_count: usize,
    /// Number of registrations carrying optional display metadata.
    active_registration_metadata_count: usize,
    /// Oldest live registration age in seconds.
    oldest_registration_age_seconds: Option<u64>,
    /// Sidecar heartbeat interval advertised to clients.
    heartbeat_interval_seconds: u64,
    /// Registration lease advertised to clients.
    registration_lease_seconds: u64,
    /// Human-readable MVP routing stage.
    routing: &'static str,
}

impl GatewayStatus {
    /// Build a fresh status snapshot.
    fn new(cfg: &RuntimeConfig, durable: &GatewayDurableState, stream_hash: String) -> Self {
        Self {
            protocol_version: SOCKET_PROTOCOL_VERSION,
            stream_hash,
            allowed_user_count: cfg.allowed_user_ids.len(),
            configured_chat_id: cfg.configured_chat_id,
            linked_chat_id: durable.linked_chat.map(|link| link.chat_id),
            next_update_offset: durable.next_update_offset,
            recent_update_count: durable.recent_update_ids.len(),
            processed_update_count: durable.processed_update_count,
            rejected_update_count: durable.rejected_update_count,
            active_sidecar_count: 0,
            active_registration_count: 0,
            active_registration_metadata_count: 0,
            oldest_registration_age_seconds: None,
            heartbeat_interval_seconds: SIDECAR_HEARTBEAT_INTERVAL.as_secs(),
            registration_lease_seconds: REGISTRATION_LEASE_DURATION.as_secs(),
            routing: "help/status-only",
        }
    }
}

/// Shared state used by the gateway local socket accept loop.
struct GatewaySocketState {
    /// Filesystem path of the private gateway socket.
    socket_path: PathBuf,
    /// Latest durable gateway status snapshot.
    status: Mutex<GatewayStatus>,
    /// Live sidecar connection and registration registry.
    registry: Mutex<GatewayRegistry>,
    /// Monotonic connection id allocator.
    next_connection_id: AtomicU64,
    /// Per-process generation that tells reconnecting sidecars to reannounce.
    generation: String,
}

impl GatewaySocketState {
    /// Build shared socket state for a newly started gateway process.
    fn new(
        cfg: &RuntimeConfig,
        durable: &GatewayDurableState,
        stream_hash: String,
        socket_path: PathBuf,
    ) -> Self {
        Self {
            socket_path,
            status: Mutex::new(GatewayStatus::new(cfg, durable, stream_hash)),
            registry: Mutex::new(GatewayRegistry::default()),
            next_connection_id: AtomicU64::new(1),
            generation: gateway_generation(),
        }
    }

    /// Replace the durable status fields after offset/state persistence.
    fn set_status(&self, mut status: GatewayStatus) {
        let counts = self.registry_counts();
        status.active_sidecar_count = counts.sidecars;
        status.active_registration_count = counts.registrations;
        status.active_registration_metadata_count = counts.registration_metadata;
        status.oldest_registration_age_seconds = counts.oldest_registration_age_seconds;
        *self.status.lock().expect("status lock") = status;
    }

    /// Allocate an id for one accepted sidecar socket.
    fn allocate_connection_id(&self) -> u64 {
        self.next_connection_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Remove expired sidecars and registration leases.
    fn prune_registry(&self) {
        self.registry
            .lock()
            .expect("registry lock")
            .prune_expired(Instant::now());
    }

    /// Return current live registry counts after removing expired leases.
    fn registry_counts(&self) -> GatewayRegistryCounts {
        let now = Instant::now();
        let mut registry = self.registry.lock().expect("registry lock");
        registry.prune_expired(now);
        registry.counts(now)
    }

    /// Build a JSON status response with live registry counters.
    fn status_response(&self, reannounce_required: bool) -> serde_json::Value {
        let counts = self.registry_counts();
        let mut status = self.status.lock().expect("status lock").clone();
        status.active_sidecar_count = counts.sidecars;
        status.active_registration_count = counts.registrations;
        status.active_registration_metadata_count = counts.registration_metadata;
        status.oldest_registration_age_seconds = counts.oldest_registration_age_seconds;
        serde_json::json!({
            "protocol_version": status.protocol_version,
            "stream_hash": status.stream_hash,
            "socket_path": self.socket_path,
            "allowed_user_count": status.allowed_user_count,
            "configured_chat_id": status.configured_chat_id,
            "linked_chat_id": status.linked_chat_id,
            "next_update_offset": status.next_update_offset,
            "recent_update_count": status.recent_update_count,
            "processed_update_count": status.processed_update_count,
            "rejected_update_count": status.rejected_update_count,
            "active_sidecar_count": status.active_sidecar_count,
            "active_registration_count": status.active_registration_count,
            "active_registration_metadata_count": status.active_registration_metadata_count,
            "oldest_registration_age_seconds": status.oldest_registration_age_seconds,
            "heartbeat_interval_seconds": status.heartbeat_interval_seconds,
            "registration_lease_seconds": status.registration_lease_seconds,
            "gateway_generation": self.generation,
            "reannounce_required": reannounce_required,
            "routing": status.routing,
        })
    }
}

/// Live registry summary used by status responses.
struct GatewayRegistryCounts {
    /// Number of connected sidecars.
    sidecars: usize,
    /// Number of live registrations.
    registrations: usize,
    /// Number of registrations with optional display/tool metadata.
    registration_metadata: usize,
    /// Oldest registration age in seconds.
    oldest_registration_age_seconds: Option<u64>,
}

/// Live sidecar registry keyed by accepted socket connection id.
#[derive(Default)]
struct GatewayRegistry {
    /// Connected sidecars and their last heartbeat time.
    sidecars: HashMap<u64, GatewaySidecar>,
    /// Registered agent routes owned by connected sidecars.
    registrations: HashMap<GatewayRegistrationKey, GatewayRegistration>,
}

impl GatewayRegistry {
    /// Add or refresh a connected sidecar.
    fn hello(&mut self, connection_id: u64, now: Instant) {
        self.sidecars
            .entry(connection_id)
            .and_modify(|sidecar| sidecar.last_seen = now)
            .or_insert(GatewaySidecar { last_seen: now });
    }

    /// Refresh a sidecar heartbeat and extend its registration leases.
    fn heartbeat(&mut self, connection_id: u64, now: Instant) -> Result<(), String> {
        let sidecar = self
            .sidecars
            .get_mut(&connection_id)
            .ok_or_else(|| "sidecar must send hello before heartbeat".to_owned())?;
        sidecar.last_seen = now;
        for registration in self.registrations.values_mut() {
            if registration.connection_id == connection_id {
                registration.expires_at = now + REGISTRATION_LEASE_DURATION;
            }
        }
        Ok(())
    }

    /// Register or refresh one `(session_id, agent_id)` route for this sidecar.
    fn register_agent(
        &mut self,
        connection_id: u64,
        request: GatewaySocketRequest,
        now: Instant,
    ) -> Result<(), String> {
        if !self.sidecars.contains_key(&connection_id) {
            return Err("sidecar must send hello before register_agent".to_owned());
        }
        let session_id = required_request_field(request.session_id, "session_id")?;
        let agent_id = required_request_field(request.agent_id, "agent_id")?;
        let key = GatewayRegistrationKey {
            session_id,
            agent_id,
        };
        self.registrations.insert(
            key,
            GatewayRegistration {
                connection_id,
                display_name: request.display_name,
                tool_namespace: request.tool_namespace,
                registered_at: now,
                expires_at: now + REGISTRATION_LEASE_DURATION,
            },
        );
        Ok(())
    }

    /// Remove one registered agent route for this sidecar.
    fn unregister_agent(
        &mut self,
        connection_id: u64,
        request: GatewaySocketRequest,
    ) -> Result<(), String> {
        let session_id = required_request_field(request.session_id, "session_id")?;
        let agent_id = required_request_field(request.agent_id, "agent_id")?;
        let key = GatewayRegistrationKey {
            session_id,
            agent_id,
        };
        if self
            .registrations
            .get(&key)
            .is_some_and(|registration| registration.connection_id == connection_id)
        {
            self.registrations.remove(&key);
        }
        Ok(())
    }

    /// Remove all routes owned by a disconnected sidecar.
    fn disconnect(&mut self, connection_id: u64) {
        self.sidecars.remove(&connection_id);
        self.registrations
            .retain(|_, registration| registration.connection_id != connection_id);
    }

    /// Return live registry counts for a status response.
    fn counts(&self, now: Instant) -> GatewayRegistryCounts {
        let oldest_registration_age_seconds = self
            .registrations
            .values()
            .map(|registration| {
                now.saturating_duration_since(registration.registered_at)
                    .as_secs()
            })
            .max();
        let registration_metadata = self
            .registrations
            .values()
            .filter(|registration| {
                registration.display_name.is_some() || registration.tool_namespace.is_some()
            })
            .count();
        GatewayRegistryCounts {
            sidecars: self.sidecars.len(),
            registrations: self.registrations.len(),
            registration_metadata,
            oldest_registration_age_seconds,
        }
    }

    /// Remove expired sidecars and registration leases.
    fn prune_expired(&mut self, now: Instant) {
        let expired_sidecars = self
            .sidecars
            .iter()
            .filter_map(|(connection_id, sidecar)| {
                (sidecar.last_seen + REGISTRATION_LEASE_DURATION <= now).then_some(*connection_id)
            })
            .collect::<Vec<_>>();
        for connection_id in expired_sidecars {
            self.disconnect(connection_id);
        }
        self.registrations
            .retain(|_, registration| registration.expires_at > now);
    }
}

/// Connected sidecar heartbeat state.
struct GatewaySidecar {
    /// Last time the sidecar sent hello or heartbeat.
    last_seen: Instant,
}

/// Key identifying one registered Tau agent route.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct GatewayRegistrationKey {
    /// Tau session id announced by the sidecar.
    session_id: String,
    /// Tau agent id announced by the sidecar.
    agent_id: String,
}

/// Live route metadata for one registered Tau agent.
struct GatewayRegistration {
    /// Sidecar connection that owns this route.
    connection_id: u64,
    /// Optional model/display name for diagnostics.
    display_name: Option<String>,
    /// Tool namespace exposed by this sidecar.
    tool_namespace: Option<String>,
    /// Registration creation time.
    registered_at: Instant,
    /// Lease expiry time extended by heartbeats.
    expires_at: Instant,
}

/// Parsed JSON request from a gateway local socket client.
#[derive(Debug, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct GatewaySocketRequest {
    /// Protocol version expected by the client.
    protocol_version: Option<u32>,
    /// Request kind such as `status`, `hello`, `heartbeat`,
    /// `register_agent`, `unregister_agent`, or `goodbye`.
    kind: String,
    /// Tau session id for registration requests.
    session_id: Option<String>,
    /// Tau agent id for registration requests.
    agent_id: Option<String>,
    /// Optional display name supplied by the sidecar.
    display_name: Option<String>,
    /// Optional tool namespace supplied by the sidecar.
    tool_namespace: Option<String>,
}

impl Default for GatewaySocketRequest {
    fn default() -> Self {
        Self {
            protocol_version: None,
            kind: "status".to_owned(),
            session_id: None,
            agent_id: None,
            display_name: None,
            tool_namespace: None,
        }
    }
}

/// Guard for a bound gateway local socket path.
struct GatewaySocketGuard {
    /// Bound Unix socket path.
    path: PathBuf,
}

impl GatewaySocketGuard {
    /// Bind a private status socket and start its accept loop.
    fn bind(path: PathBuf, state: Arc<GatewaySocketState>) -> Result<Self, String> {
        remove_inactive_socket(&path)?;
        let listener = UnixListener::bind(&path)
            .map_err(|error| format!("binding Telegram gateway socket: {error}"))?;
        thread::Builder::new()
            .name("telegram-gateway-socket".to_owned())
            .spawn(move || accept_gateway_socket_loop(listener, state))
            .map_err(|error| format!("starting Telegram gateway socket thread: {error}"))?;
        Ok(Self { path })
    }
}

impl Drop for GatewaySocketGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Accept local status socket clients until the process exits.
fn accept_gateway_socket_loop(listener: UnixListener, state: Arc<GatewaySocketState>) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let state = Arc::clone(&state);
                let _ = thread::Builder::new()
                    .name("telegram-gateway-client".to_owned())
                    .spawn(move || handle_gateway_socket_client(stream, state));
            }
            Err(error) => {
                tracing::warn!(target: crate::LOG_TARGET, error = %error, "telegram gateway socket accept failed");
            }
        }
    }
}

/// Handle one local socket client using JSON-line requests and responses.
fn handle_gateway_socket_client(mut stream: UnixStream, state: Arc<GatewaySocketState>) {
    let connection_id = state.allocate_connection_id();
    let _ = stream.set_read_timeout(Some(REGISTRATION_LEASE_DURATION));
    loop {
        state.prune_registry();
        let mut close_after_response;
        let response = match read_gateway_socket_request(&stream) {
            Ok(Some(request)) => {
                close_after_response = request.kind == "status";
                handle_gateway_socket_request(&state, connection_id, request)
            }
            Ok(None) => break,
            Err(error) => {
                close_after_response = true;
                serde_json::json!({
                    "protocol_version": SOCKET_PROTOCOL_VERSION,
                    "ok": false,
                    "error": error,
                })
            }
        };
        if response.get("ok").and_then(serde_json::Value::as_bool) == Some(false) {
            close_after_response = true;
        }
        if !write_gateway_socket_response(&mut stream, &response) {
            break;
        }
        if close_after_response
            || response.get("goodbye").and_then(serde_json::Value::as_bool) == Some(true)
        {
            break;
        }
    }
    state
        .registry
        .lock()
        .expect("registry lock")
        .disconnect(connection_id);
}

/// Apply one parsed local socket request to the gateway registry.
fn handle_gateway_socket_request(
    state: &GatewaySocketState,
    connection_id: u64,
    request: GatewaySocketRequest,
) -> serde_json::Value {
    match request.kind.as_str() {
        "status" => state.status_response(false),
        "hello" => {
            state
                .registry
                .lock()
                .expect("registry lock")
                .hello(connection_id, Instant::now());
            state.status_response(true)
        }
        "heartbeat" => registry_result(state, |registry| {
            registry.heartbeat(connection_id, Instant::now())
        }),
        "register_agent" => registry_result(state, |registry| {
            registry.register_agent(connection_id, request, Instant::now())
        }),
        "unregister_agent" => registry_result(state, |registry| {
            registry.unregister_agent(connection_id, request)
        }),
        "goodbye" => serde_json::json!({
            "protocol_version": SOCKET_PROTOCOL_VERSION,
            "ok": true,
            "goodbye": true,
        }),
        kind => serde_json::json!({
            "protocol_version": SOCKET_PROTOCOL_VERSION,
            "ok": false,
            "error": format!("unsupported gateway socket request kind `{kind}`"),
        }),
    }
}

/// Execute a registry mutation and return a JSON result response.
fn registry_result<F>(state: &GatewaySocketState, f: F) -> serde_json::Value
where
    F: FnOnce(&mut GatewayRegistry) -> Result<(), String>,
{
    let result = {
        let mut registry = state.registry.lock().expect("registry lock");
        registry.prune_expired(Instant::now());
        f(&mut registry)
    };
    match result {
        Ok(()) => serde_json::json!({
            "protocol_version": SOCKET_PROTOCOL_VERSION,
            "ok": true,
            "heartbeat_interval_seconds": SIDECAR_HEARTBEAT_INTERVAL.as_secs(),
            "registration_lease_seconds": REGISTRATION_LEASE_DURATION.as_secs(),
            "gateway_generation": state.generation,
        }),
        Err(error) => serde_json::json!({
            "protocol_version": SOCKET_PROTOCOL_VERSION,
            "ok": false,
            "error": error,
        }),
    }
}

/// Write one JSON-line response to a gateway socket client.
fn write_gateway_socket_response(stream: &mut UnixStream, response: &serde_json::Value) -> bool {
    match serde_json::to_string(response) {
        Ok(text) => writeln!(stream, "{text}")
            .and_then(|()| stream.flush())
            .is_ok(),
        Err(_) => false,
    }
}

/// Read and validate one JSON-line local socket request.
fn read_gateway_socket_request(
    stream: &UnixStream,
) -> Result<Option<GatewaySocketRequest>, String> {
    let mut reader = stream
        .try_clone()
        .map_err(|error| format!("cloning gateway socket stream: {error}"))?;
    let mut request = Vec::new();
    loop {
        let mut byte = [0_u8; 1];
        match reader.read(&mut byte) {
            Ok(0) => {
                if request.is_empty() {
                    return Ok(None);
                }
                break;
            }
            Ok(_) => {
                request.push(byte[0]);
                if request.len() > MAX_SOCKET_REQUEST_BYTES {
                    return Err("gateway socket request is too large".to_owned());
                }
                if byte[0] == b'\n' {
                    break;
                }
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                if request.is_empty() {
                    return Ok(None);
                }
                break;
            }
            Err(error) => return Err(format!("reading gateway socket request: {error}")),
        }
    }
    let line = std::str::from_utf8(&request)
        .map_err(|error| format!("gateway socket request is not UTF-8: {error}"))?;
    let parsed: GatewaySocketRequest = serde_json::from_str(line)
        .map_err(|error| format!("invalid gateway socket JSON request: {error}"))?;
    if parsed.protocol_version.unwrap_or(SOCKET_PROTOCOL_VERSION) != SOCKET_PROTOCOL_VERSION {
        return Err(format!(
            "unsupported gateway socket protocol version {}",
            parsed.protocol_version.unwrap_or_default()
        ));
    }
    Ok(Some(parsed))
}

/// Extract and validate a required sidecar request field.
fn required_request_field(value: Option<String>, name: &str) -> Result<String, String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("gateway socket request requires `{name}`"))
}

/// Return a per-process gateway generation label for reconnect detection.
fn gateway_generation() -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{timestamp:x}")
}

/// Create a private directory, rejecting symlink final components.
fn create_private_dir(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|error| format!("creating private Telegram gateway directory: {error}"))?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("inspecting Telegram gateway directory: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!(
            "Telegram gateway path {} is not a real directory",
            path.display()
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("setting Telegram gateway directory permissions: {error}"))
}

/// Remove an inactive stale socket while refusing active or non-socket paths.
fn remove_inactive_socket(path: &Path) -> Result<(), String> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if !metadata.file_type().is_socket() {
        return Err(format!(
            "refusing to replace non-socket Telegram gateway path {}",
            path.display()
        ));
    }
    match UnixStream::connect(path) {
        Ok(_) => Err(format!(
            "refusing to replace active Telegram gateway socket {}",
            path.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
            fs::remove_file(path)
                .map_err(|error| format!("removing stale Telegram gateway socket: {error}"))
        }
        Err(error) => Err(format!(
            "could not prove Telegram gateway socket {} is stale: {error}",
            path.display()
        )),
    }
}

/// Return the default state directory.
fn default_state_dir<F>(get_env: &F) -> PathBuf
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(xdg_state_home) = get_env("XDG_STATE_HOME").filter(|value| !value.trim().is_empty())
    {
        return PathBuf::from(xdg_state_home)
            .join("tau")
            .join("ext")
            .join("telegram-gateway");
    }
    PathBuf::from(
        get_env("HOME")
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| ".".to_owned()),
    )
    .join(".local")
    .join("state")
    .join("tau")
    .join("ext")
    .join("telegram-gateway")
}

/// Return the default runtime socket directory.
fn default_runtime_dir<F>(get_env: &F, state_dir: &Path) -> PathBuf
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(xdg_runtime_dir) =
        get_env("XDG_RUNTIME_DIR").filter(|value| !value.trim().is_empty())
    {
        return PathBuf::from(xdg_runtime_dir)
            .join("tau")
            .join("telegram-gateway");
    }
    state_dir.join("run")
}

/// Parse one signed integer command-line flag value.
fn parse_i64_flag(flag: &str, value: &str) -> Result<i64, String> {
    value
        .trim()
        .parse()
        .map_err(|_| format!("{flag} expects an integer value"))
}

/// Parse one unsigned integer command-line flag value.
fn parse_u64_flag(flag: &str, value: &str) -> Result<u64, String> {
    value
        .trim()
        .parse()
        .map_err(|_| format!("{flag} expects an unsigned integer value"))
}

/// Return concise usage text for the MVP daemon.
fn gateway_usage() -> String {
    "Usage: tau-telegram-gateway --allowed-user-id <telegram-user-id> [--allowed-user-id <id> ...] [--allowed-user-ids <id,id>] [--bot-token-env TELEGRAM_BOT_TOKEN] [--chat-id <chat-id>] [--api-base <url>] [--poll-timeout-seconds <seconds>] [--state-dir <path>] [--runtime-dir <path>]".to_owned()
}

/// Return help text sent to Telegram users.
fn gateway_help_text() -> &'static str {
    "Tau Telegram gateway MVP is running. Supported commands: /start, /help, /status. Prompt routing to Tau sessions/agents is not enabled in this slice yet."
}

/// Return text for non-command messages before routing exists.
fn gateway_not_routing_text() -> &'static str {
    "Telegram gateway MVP is online, but prompt routing is not implemented yet. Use /help or /status."
}

/// Bound a Telegram reply to the MVP output limit.
fn bounded_reply_text(text: &str) -> String {
    if text.len() <= MAX_REPLY_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_REPLY_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests;
