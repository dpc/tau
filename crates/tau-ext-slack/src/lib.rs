//! Personal Slack Socket Mode bridge extension for Tau agents.
//!
//! The extension declares logical `slack_register`, `slack_conversations`,
//! `slack_send`, and separately authorized `slack_react` tools,
//! which `ToolNameScope` maps to final per-instance wire names. Proactive
//! destination authorization follows `DESIGN-tau-ext-slack-proactive-sends`. It
//! is disabled by default, requires Slack token secrets plus a non-empty
//! allowlist, and treats Slack text as external untrusted prompt input.
//! Reply routing follows
//! `DESIGN-tau-ext-slack-canonical-reply-selectors`.
//! Outbound retry and replay follow
//! `DESIGN-tau-ext-slack-send-delivery`.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::error::Error;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use rand::RngCore as _;
use tau_client::{ClientError, ClientHandle, ClientResult, ExtensionBuilder, TauExtension};
use tau_proto::{
    AgentId, CborValue, CompleteTransportSendRequest, ConversationKind, Event, ExternalActorKind,
    ExternalMessageIdentity, HarnessInputMessage, HarnessNotice, MessageConversation,
    MessageEndpoint, MessageId, MessageOperation, MessagePayload, MessageReaction, MessageRef,
    MessageThread, MessageTransportAcceptance, NoticeLevel, ReactionAction,
    RegisterTransportCapabilityRequest, SenderIdentityAssurance, SenderPolicyStatus, TextFormat,
    ToolError, ToolExample, ToolProgress, ToolResult, ToolSpec, ToolStarted, ToolUseState,
    ToolUseStatus, TransportMessageDraft, TransportMessageIngressRequest,
    TransportSendDestinationCapability,
};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

mod admission;
mod posted_message_cache;
mod send_delivery;

use admission::{AdmissionQueue, QueueDepthBucket, ReserveError};
use posted_message_cache::{PostedMessageCache, PostedMessageKey, PostedMessageOwner};
use send_delivery::{
    CompletionOutput, FrozenPostBody, FrozenSendAuthority, PostAttemptFailure, PostAttemptOutcome,
    RemoteCopyPossibility, SendFailureCategory, SendLedgerDisposition, SendLedgerEntry,
    SendQueueReservation, SendScheduler, SendWake, SlackApiError, SlackPostMode,
    SystemSendScheduler, classify_api_error, classify_post_api_error, parse_retry_after,
};

/// Tracing target used by this extension.
pub const LOG_TARGET: &str = "slack";

/// Record an expected fail-closed ingress decision without untrusted
/// identifiers.
fn log_ingress_rejection(category: &'static str) {
    tracing::debug!(target: LOG_TARGET, rejection = category, "Slack ingress occurrence rejected");
}

/// Record a successful ACK without including its untrusted envelope identity.
fn log_socket_ack_sent(has_supported_event: bool) {
    tracing::debug!(target: LOG_TARGET, ack = "sent", has_supported_event, "Slack Socket Mode envelope acknowledged");
}

/// Finish one ACK attempt, surfacing failure without untrusted envelope data.
fn finish_socket_ack(result: Result<(), String>, has_supported_event: bool) -> Result<(), String> {
    match result {
        Ok(()) => {
            log_socket_ack_sent(has_supported_event);
            Ok(())
        }
        Err(error) => {
            tracing::warn!(target: LOG_TARGET, lifecycle = "degraded", ack = "failed", has_supported_event, "Slack Socket Mode envelope acknowledgement failed");
            Err(error)
        }
    }
}

/// Logical tool name for registering the current agent as a Slack listener.
pub const REGISTER_TOOL_NAME: &str = "slack_register";

/// Logical tool name for discovering configured Slack conversation aliases.
pub const CONVERSATIONS_TOOL_NAME: &str = "slack_conversations";

/// Logical tool name for sending a Slack message from a registered agent.
pub const SEND_TOOL_NAME: &str = "slack_send";
/// Logical tool name for adding or removing one source-bound Slack reaction.
pub const REACT_TOOL_NAME: &str = "slack_react";

/// Logical tool group name shared by all Slack bridge tools.
pub const TOOL_GROUP_NAME: &str = "slack";

/// Tag marking tools that register an agent with the Slack bridge.
pub const REGISTER_TOOL_TAG: &str = "slack:register";

/// Tag marking tools that disclose configured Slack conversation policy.
pub const CONVERSATIONS_TOOL_TAG: &str = "slack:discover";

/// Tag marking tools that send messages through the Slack bridge.
pub const SEND_TOOL_TAG: &str = "slack:send";
/// Tag marking the separately authorized outbound reaction surface.
pub const REACT_TOOL_TAG: &str = "slack:react";

const DEFAULT_API_BASE: &str = "https://slack.com/api";
const DEFAULT_MAX_MESSAGE_BYTES: usize = 16 * 1024;
const MAX_MESSAGE_BYTES: usize = 128 * 1024;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const SEND_ATTEMPT_HORIZON: Duration = Duration::from_secs(60);
const MAX_SLACK_API_RESPONSE_BYTES: u64 = 64 * 1024;
const DUPLICATE_CACHE_SIZE: usize = 1024;
const POSTED_MESSAGE_CACHE_SIZE: usize = 1024;
const ROUTE_CORRELATION_LIMIT: usize = 1024;
const REPLY_ROUTE_LIMIT: usize = 1024;
const PENDING_SEND_LIMIT: usize = 1024;
const SEND_LEDGER_LIMIT: usize = PENDING_SEND_LIMIT;
const ACTIVE_SEND_WORKER_LIMIT: usize = 64;
// Ownership can pin at most `REACTION_OWNERSHIP_LIMIT` entries; the additional
// pending-send headroom guarantees every accepted send completion can activate
// without evicting a pinned target.
const REACTION_TARGET_LIMIT: usize = REACTION_OWNERSHIP_LIMIT + PENDING_SEND_LIMIT;
const REACTION_OWNERSHIP_LIMIT: usize = 1024;
const REACTION_ATTEMPT_LIMIT: usize = 256;
const CONVERSATION_LIMIT: usize = 64;
const CONVERSATION_ALIAS_PATTERN: &str = "^[a-z][a-z0-9_-]{0,63}$";
const MAX_CONVERSATION_ALIAS_BYTES: usize = 64;
const DEFAULT_DISCOVERY_PAGE_LIMIT: usize = 20;
const MAX_DISCOVERY_PAGE_LIMIT: usize = 32;
const MAX_DISCOVERY_CURSOR_BYTES: usize = 128;
const MAX_DISCOVERY_RESULT_BYTES: usize = 24 * 1024;
const DYNAMIC_DM_LIMIT: usize = 64;
const DYNAMIC_DM_LABEL: &str = "direct-message";
const TRANSPORT_NAME: &str = "slack";
const CAPABILITY_REQUEST_PREFIX: &str = "slack-capability-";
const MAX_DIAGNOSTIC_BYTES: usize = 512;
const MAX_SOCKET_FRAME_BYTES: usize = 256 * 1024;
const INITIAL_RECONNECT_BACKOFF: Duration = Duration::from_secs(1);
const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(30);
const LATENCY_SCHEMA: &str = "slack_latency_v1";

/// Explicit private worker context used for timing and stale-authority checks.
struct AdmissionContext {
    /// Payload-free latency correlation.
    trace: LatencyTrace,
    /// Local websocket receive instant for frame-to-submit attribution.
    received_at: Instant,
    /// Lifecycle authority captured before ACK.
    ingress_epoch: u64,
    /// Configuration generation captured before ACK.
    config_generation: u64,
    /// Agent-registration generation captured before ACK for local effects.
    agent_generation: u64,
    /// Exact authenticated installation team captured before ACK.
    installation_team_id: String,
    /// Time spent waiting after successful ACK handoff.
    queue_wait_us: u64,
    /// Time spent in live identity verification.
    identity_us: Cell<u64>,
    /// Explicit terminal class selected by the operation that owns the outcome.
    outcome: Cell<&'static str>,
}

/// Payload-free fields shared by one occurrence's latency markers.
#[derive(Clone, Copy)]
struct LatencyTrace {
    /// Socket generation local to this extension process.
    connection_generation: u64,
    /// Occurrence ordinal local to this extension process.
    trace_seq: u64,
    /// Stable low-cardinality decoded event class.
    event_class: &'static str,
}

impl AdmissionContext {
    /// Check lifecycle authority while the caller already owns the state lock.
    fn matches_state(&self, state: &State) -> bool {
        state.ingress_epoch == self.ingress_epoch
            && state.config_generation == self.config_generation
            && state.installation_team_id.as_deref() == Some(self.installation_team_id.as_str())
    }

    /// Check lifecycle plus agent registration authority for local effects.
    fn matches_local_state(&self, state: &State) -> bool {
        self.matches_state(state) && state.agent_generation == self.agent_generation
    }

    /// Set the bounded terminal outcome owned by the current operation.
    fn mark(&self, outcome: &'static str) {
        self.outcome.set(outcome);
    }

    /// Return the receive instant used by submit timing.
    fn trace_received_at(&self) -> Instant {
        self.received_at
    }
}

/// Convert a monotonic duration to a saturating integer microsecond field.
fn elapsed_us(started_at: Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_micros()).unwrap_or(u64::MAX)
}

/// Run the Slack extension over stdio.
pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    tau_client::init_logging_for(LOG_TARGET);
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
    fn open_socket(&self, cfg: &RuntimeConfig) -> Result<String, SlackApiError>;

    /// Return the exact bot-user and installation-team binding from
    /// `auth.test`.
    fn auth_test(&self, cfg: &RuntimeConfig) -> Result<SlackInstallationIdentity, SlackApiError>;

    /// Return the verified live human plus bounded presentation from one
    /// lookup.
    fn verified_human_identity(
        &self,
        cfg: &RuntimeConfig,
        user_id: &str,
    ) -> Result<Option<VerifiedSlackHuman>, SlackApiError>;

    /// Execute one exact frozen `chat.postMessage` body.
    fn post_message(
        &self,
        cfg: &RuntimeConfig,
        body: &FrozenPostBody,
    ) -> PostAttemptOutcome<PostedMessage>;

    /// Add or remove the bot's reaction on one exact cached Slack item.
    fn react(
        &self,
        _cfg: &RuntimeConfig,
        _action: ReactionActionKind,
        _channel_id: &str,
        _message_ts: &str,
        _emoji: &str,
    ) -> Result<(), ReactionApiError> {
        Err(ReactionApiError::OutcomeUnknown)
    }
}

/// Exact bot and installing workspace returned by one `auth.test`.
#[derive(Clone, Debug, Eq, PartialEq)]
struct SlackInstallationIdentity {
    /// Exact bot U/W identity authenticated by the configured bot token.
    bot_user_id: String,
    /// Exact installing T workspace authenticated by the configured bot token.
    team_id: String,
}

/// Exact verified Slack account plus its UI-only display snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
struct VerifiedSlackHuman {
    /// Exact live-human U/W identity returned by `users.info`.
    user_id: String,
    /// Optional bounded, presentation-only Slack display snapshot.
    display_name: Option<String>,
}

/// Explicit outbound reaction operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReactionActionKind {
    /// Add the named reaction.
    Add,
    /// Remove the named reaction.
    Remove,
}

impl ReactionActionKind {
    /// Parse the exact public action spelling.
    fn parse(value: &str) -> Option<Self> {
        match value {
            "add" => Some(Self::Add),
            "remove" => Some(Self::Remove),
            _ => None,
        }
    }

    /// Return the exact public action spelling.
    fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Remove => "remove",
        }
    }
}

/// Safe, typed outcomes from Slack's reaction methods.
#[derive(Clone, Debug, Eq, PartialEq)]
enum ReactionApiError {
    /// Slack reports the bot already has the reaction.
    AlreadyReacted,
    /// Slack reports the bot has no such reaction.
    NoReaction,
    /// Slack throttled the request for this bounded duration.
    RateLimited(u64),
    /// The app lacks the separately documented write scope.
    MissingScope,
    /// A definitive bounded Slack error category.
    Definitive(&'static str),
    /// The remote effect may have happened.
    OutcomeUnknown,
}

/// Stable identity returned by Slack for one successfully posted message.
#[derive(Clone, Debug, Eq, PartialEq)]
struct PostedMessage {
    /// Conversation id returned by Slack for the accepted post.
    channel_id: String,
    /// Slack message timestamp used as its stable id.
    ts: String,
    /// Root thread timestamp when Slack reports this post as a reply.
    thread_ts: Option<String>,
}

/// Validated runtime configuration, including resolved secret values.
#[derive(Clone)]
struct RuntimeConfig {
    /// Resolved Slack app-level token. Never log this value.
    app_token: String,
    /// Resolved Slack bot token. Never log this value.
    bot_token: String,
    /// Slack user ids explicitly allowlisted for ingress and bridge-control
    /// commands.
    allowed_user_ids: HashSet<String>,
    /// Presentation aliases keyed one-to-one by exact native user id.
    sender_aliases: HashMap<String, String>,
    /// Policy controlling which verified human senders may enter Tau.
    security_mode: SecurityMode,
    /// Exact validated static conversation policies, keyed by stable alias.
    conversations: BTreeMap<String, ConversationPolicy>,
    /// Receive-enabled parent routes keyed by native conversation id.
    parent_receives: HashMap<String, String>,
    /// Receive-enabled fixed routes keyed by native conversation and root.
    thread_receives: HashMap<(String, String), String>,
    /// Proactive aliases, independently derived from static policies.
    proactive_aliases: BTreeSet<String>,
    /// Optional bounded dynamic-DM discovery policy.
    dynamic_direct_messages: Option<DynamicDirectMessages>,
    /// Whether agent-authored posts include the originating agent id prefix.
    prefix_agent_id: bool,
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
    /// Explicitly allowlisted Slack user ids; only these may run bridge-control
    /// commands.
    allowed_user_ids: Vec<String>,
    /// Optional presentation-only sender aliases.
    sender_aliases: Vec<RawSenderAlias>,
    /// Ingress sender policy. Omission deliberately preserves strict behavior.
    security_mode: SecurityMode,
    /// Exact static receive/proactive conversation policy.
    conversations: Vec<RawConversationPolicy>,
    /// Optional bounded dynamic one-to-one DM discovery policy.
    dynamic_direct_messages: Option<DynamicDirectMessages>,
    /// Whether to prefix agent-authored posts with `[agent-id] `.
    prefix_agent_id: bool,
    /// Removed key retained only to produce actionable migration errors.
    #[serde(deserialize_with = "removed_config_key")]
    channel_ids: bool,
    /// Removed key retained only to produce actionable migration errors.
    #[serde(deserialize_with = "removed_config_key")]
    listening_scope: bool,
    /// Removed key retained only to produce actionable migration errors.
    #[serde(deserialize_with = "removed_config_key")]
    send_destinations: bool,
    /// Optional Slack Web API base URL, mostly for tests.
    api_base: Option<String>,
    /// Optional maximum accepted text size in bytes.
    max_message_bytes: Option<usize>,
}

/// One operator-owned presentation alias.
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSenderAlias {
    /// Exact native Slack U/W identity receiving the alias.
    user_id: String,
    /// Operator-owned, presentation-only alias.
    alias: String,
}

/// Mark any encountered removed key as present, including an explicit null.
fn removed_config_key<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    <serde::de::IgnoredAny as serde::Deserialize>::deserialize(deserializer)?;
    Ok(true)
}

/// Raw operator-configured static Slack conversation policy.
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConversationPolicy {
    /// Stable lower-case model-facing name.
    alias: String,
    /// Existing Slack conversation id, never a user id.
    conversation_id: String,
    /// Expected Slack conversation family.
    kind: ConversationPolicyKind,
    /// Optional inbound trigger permission.
    receive: Option<ReceiveMode>,
    /// Whether proactive initiation to this alias is authorized.
    #[serde(default)]
    proactive_send: bool,
    /// Optional trusted model-facing operator hint.
    description: Option<String>,
    /// Optional immutable root thread timestamp.
    thread_ts: Option<String>,
}

/// Supported static Slack conversation families.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum ConversationPolicyKind {
    /// Public or private Slack channel.
    Channel,
    /// Existing multi-person direct conversation.
    Mpim,
    /// Existing one-to-one direct conversation.
    Dm,
}

/// Fully validated static receive/proactive route.
#[derive(Clone)]
struct ConversationPolicy {
    /// Stable model-facing route label.
    alias: String,
    /// Exact existing native Slack conversation.
    conversation_id: String,
    /// Explicit native conversation family.
    kind: ConversationPolicyKind,
    /// Optional inbound trigger permission.
    receive: Option<ReceiveMode>,
    /// Optional trusted model-facing operator hint.
    description: Option<String>,
    /// Optional immutable fixed-thread root.
    thread_ts: Option<String>,
}

/// Inbound trigger mode for one exact static route.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReceiveMode {
    /// Accept Slack `app_mention` events for the route.
    MentionsOnly,
    /// Accept all eligible messages in the exact route.
    AllMessages,
}

/// Optional dynamic one-to-one DM discovery settings.
#[derive(Clone, Copy, Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DynamicDirectMessages {
    /// DMs always receive ordinary `message.im` events.
    receive: ReceiveMode,
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
        if self.channel_ids || self.listening_scope || self.send_destinations {
            return Err(
                "Slack `channel_ids`, `listening_scope`, and `send_destinations` were removed; migrate each exact route to `conversations[]` with optional `receive` and `proactive_send`"
                    .to_owned(),
            );
        }
        let mut allowed_user_ids = HashSet::new();
        for user_id in self.allowed_user_ids {
            let user_id = validate_user_id("allowed_user_ids", &user_id)?;
            if !allowed_user_ids.insert(user_id.clone()) {
                return Err(format!(
                    "slack `allowed_user_ids` contains duplicate id `{user_id}`"
                ));
            }
        }
        if self.sender_aliases.len() > 64 {
            return Err("slack `sender_aliases` supports at most 64 entries".to_owned());
        }
        let mut sender_aliases = HashMap::new();
        let mut alias_values = HashSet::new();
        for raw in self.sender_aliases {
            if !valid_conversation_alias(&raw.alias) || !alias_values.insert(raw.alias.clone()) {
                return Err(
                    "slack sender aliases must be unique and match the conversation alias grammar"
                        .to_owned(),
                );
            }
            let user_id = validate_user_id("sender_aliases[].user_id", &raw.user_id)?;
            if sender_aliases.insert(user_id, raw.alias).is_some() {
                return Err("slack `sender_aliases` contains a duplicate user id".to_owned());
            }
        }
        if self.conversations.len() > CONVERSATION_LIMIT {
            return Err(format!(
                "slack `conversations` supports at most {CONVERSATION_LIMIT} entries"
            ));
        }
        let mut conversations = BTreeMap::new();
        let mut proactive_aliases = BTreeSet::new();
        let mut routes = HashSet::new();
        let mut kinds = HashMap::new();
        let mut parent_receives = HashMap::new();
        let mut thread_receives = HashMap::new();
        for raw in self.conversations {
            if !valid_conversation_alias(&raw.alias) {
                return Err(format!(
                    "slack conversation alias must match {CONVERSATION_ALIAS_PATTERN}"
                ));
            }
            if raw.alias == DYNAMIC_DM_LABEL {
                return Err(format!(
                    "slack conversation alias `{DYNAMIC_DM_LABEL}` is reserved for dynamic DMs"
                ));
            }
            let conversation_id =
                validate_conversation_id("conversations[].conversation_id", &raw.conversation_id)?;
            let valid_prefix = match raw.kind {
                ConversationPolicyKind::Channel => {
                    matches!(conversation_id.as_bytes().first(), Some(b'C' | b'G'))
                }
                ConversationPolicyKind::Mpim => conversation_id.starts_with('G'),
                ConversationPolicyKind::Dm => conversation_id.starts_with('D'),
            };
            if !valid_prefix {
                return Err("slack conversation kind does not match conversation id".to_owned());
            }
            if let Some(previous) = kinds.insert(conversation_id.clone(), raw.kind)
                && previous != raw.kind
            {
                return Err(
                    "slack conversation id cannot have conflicting `kind` values".to_owned(),
                );
            }
            if let Some(thread_ts) = &raw.thread_ts
                && validate_slack_ts(thread_ts).is_err()
            {
                return Err("slack conversation has invalid `thread_ts`".to_owned());
            }
            if raw.receive == Some(ReceiveMode::MentionsOnly)
                && raw.kind == ConversationPolicyKind::Dm
            {
                return Err("slack DM receive must be `all_messages`".to_owned());
            }
            if raw.receive.is_none() && !raw.proactive_send {
                return Err(
                    "slack conversation must enable `receive` and/or `proactive_send`".to_owned(),
                );
            }
            let description = raw
                .description
                .map(|value| {
                    if value.trim().is_empty()
                        || value.chars().count() > 120
                        || value.chars().any(char::is_control)
                    {
                        Err("slack conversation has invalid `description`".to_owned())
                    } else {
                        Ok(value)
                    }
                })
                .transpose()?;
            if !routes.insert((conversation_id.clone(), raw.thread_ts.clone())) {
                return Err(
                    "slack `conversations` contains a duplicate exact native route".to_owned(),
                );
            }
            let policy = ConversationPolicy {
                alias: raw.alias.clone(),
                conversation_id,
                kind: raw.kind,
                receive: raw.receive,
                description,
                thread_ts: raw.thread_ts,
            };
            if raw.proactive_send {
                proactive_aliases.insert(raw.alias.clone());
            }
            if policy.receive.is_some() {
                if let Some(thread_ts) = &policy.thread_ts {
                    thread_receives.insert(
                        (policy.conversation_id.clone(), thread_ts.clone()),
                        policy.alias.clone(),
                    );
                } else {
                    parent_receives.insert(policy.conversation_id.clone(), policy.alias.clone());
                }
            }
            if conversations.insert(raw.alias, policy).is_some() {
                return Err("slack `conversations` contains a duplicate alias".to_owned());
            }
        }
        if parent_receives.keys().any(|parent| {
            thread_receives
                .keys()
                .any(|(conversation, _)| conversation == parent)
        }) {
            return Err(
                "slack receive-enabled parent overlaps a receive-enabled fixed thread; remove one `receive`"
                    .to_owned(),
            );
        }
        if let Some(dynamic) = self.dynamic_direct_messages
            && dynamic.receive != ReceiveMode::AllMessages
        {
            return Err("slack dynamic DM receive must be `all_messages`".to_owned());
        }
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
        if conversations.is_empty() && self.dynamic_direct_messages.is_none() {
            return Err(
                "slack conversation policy is inactive; configure `conversations` and/or `dynamic_direct_messages`"
                    .to_owned(),
            );
        }
        Ok(RuntimeConfig {
            app_token: app_token.to_owned(),
            bot_token: bot_token.to_owned(),
            allowed_user_ids,
            sender_aliases,
            security_mode: self.security_mode,
            conversations,
            parent_receives,
            thread_receives,
            proactive_aliases,
            dynamic_direct_messages: self.dynamic_direct_messages,
            prefix_agent_id: self.prefix_agent_id,
            api_base,
            max_message_bytes,
        })
    }
}

fn valid_conversation_alias(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some('a'..='z'))
        && value.len() <= MAX_CONVERSATION_ALIAS_BYTES
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '_' | '-'))
}

/// Slack ingress sender policy.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum SecurityMode {
    /// Admit only explicitly allowlisted verified humans.
    #[default]
    Strict,
    /// Also admit verified humans in an already authorized conversation.
    Lax,
}

impl RuntimeConfig {
    /// Classify an admitted sender, or deny it under the configured policy.
    ///
    /// Kept independent from listening scope per
    /// `DESIGN-tau-ext-slack-sender-admission`.
    fn sender_policy(&self, user_id: &str) -> Option<SenderPolicyStatus> {
        if self.allowed_user_ids.contains(user_id) {
            Some(SenderPolicyStatus::Allowlisted)
        } else if self.security_mode == SecurityMode::Lax {
            Some(SenderPolicyStatus::LaxPermitted)
        } else {
            None
        }
    }
}

/// Verified Slack sender and its already-evaluated ingress policy.
struct IngressSender {
    /// Transport-stable Slack user id.
    user_id: String,
    /// Bounded UI-only `profile.display_name` snapshot.
    display_name: Option<String>,
    /// Operator-configured presentation-only alias.
    identity_alias: Option<String>,
    /// Policy classification established before submission.
    policy_status: SenderPolicyStatus,
}

/// Fully normalized Slack occurrence ready for canonical typed ingress.
struct IngressSubmission {
    /// Exact currently authorized native conversation.
    conversation: SlackConversation,
    /// Live registered agent selected for the occurrence.
    agent_id: AgentId,
    /// Verified sender and policy.
    sender: IngressSender,
    /// Immutable create/edit/reaction operation.
    operation: MessageOperation,
    /// Native identity used by durable harness deduplication.
    external_identity: ExternalMessageIdentity,
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
    /// Root thread timestamp when this message is a thread reply.
    thread_ts: Option<String>,
}

/// Slack conversation route derived exclusively from authenticated inbound
/// context.
///
/// The conversation is configured or linked when captured, and `thread_ts` is
/// absent or passed `validate_slack_ts`; neither value comes from model input.
/// Route authorization is checked again immediately before every send. See
/// `DESIGN-tau-ext-slack-immutable-thread-destinations`.
#[derive(Clone, Eq, PartialEq)]
struct SlackConversation {
    /// Exact configured or linked native conversation id.
    channel_id: String,
    /// Root thread timestamp, absent for a top-level conversation.
    thread_ts: Option<String>,
    /// Authenticated Slack conversation family.
    kind: ConversationPolicyKind,
    /// Stable configured alias, or the reserved dynamic-DM label.
    alias: String,
}

impl SlackConversation {
    /// Stable key isolating each static route and each dynamic DM's selection.
    fn route_key(&self) -> SelectionRouteKey {
        if self.alias == DYNAMIC_DM_LABEL {
            SelectionRouteKey::DynamicDm(self.channel_id.clone())
        } else {
            SelectionRouteKey::StaticAlias(self.alias.clone())
        }
    }
}

/// Exact policy scope owning one selected-agent choice.
#[derive(Clone, Eq, Hash, PartialEq)]
enum SelectionRouteKey {
    /// One configured static route, including its parent/thread scope.
    StaticAlias(String),
    /// One dynamically linked direct conversation.
    DynamicDm(String),
}

/// Slack reaction event awaiting validation and authorization.
struct SlackReaction {
    /// Slack event id used for retry suppression.
    event_id: Option<String>,
    /// Reaction lifecycle kind.
    event_type: ReactionKind,
    /// Slack user who changed the reaction.
    user_id: String,
    /// Slack reaction name without surrounding colons.
    reaction: String,
    /// Conversation containing the reacted-to post.
    channel_id: String,
    /// Stable timestamp/id of the reacted-to post.
    message_ts: String,
    /// Optional root thread timestamp reported by Slack.
    thread_ts: Option<String>,
}

/// Validated Slack `message_changed` mutation.
#[derive(Clone)]
struct SlackEdit {
    /// Slack event id used for durable occurrence deduplication.
    event_id: Option<String>,
    /// Authorized conversation containing the original message.
    channel_id: String,
    /// Verified account that performed the edit.
    editor_user_id: String,
    /// Replacement plain-text body.
    text: String,
    /// Stable timestamp/id of the original logical message.
    message_ts: String,
    /// Optional original thread root.
    thread_ts: Option<String>,
    /// Slack edit timestamp identifying this revision.
    revision_ts: String,
}

/// Supported Slack reaction lifecycle kinds.
#[derive(Clone, Copy)]
enum ReactionKind {
    /// A reaction was added.
    Added,
    /// A reaction was removed.
    Removed,
}

impl ReactionKind {
    /// Return the stable Slack event name.
    fn as_str(self) -> &'static str {
        match self {
            Self::Added => "reaction_added",
            Self::Removed => "reaction_removed",
        }
    }
}

/// One decoded Socket Mode event supported by this bridge.
enum DecodedSlackEvent {
    /// A text message or app mention.
    Message(SlackMessage),
    /// A reaction awaiting validation and authorization.
    Reaction(SlackReaction),
    /// Immutable edit occurrence referencing an earlier create.
    Edit(SlackEdit),
}

/// Bounded Socket Mode envelope classes used by payload-free traces.
#[derive(Clone, Copy, Default, Eq, PartialEq)]
enum EnvelopeClass {
    /// JSON could not be decoded.
    #[default]
    Malformed,
    /// Events API callback envelope.
    EventsApi,
    /// Connection hello envelope.
    Hello,
    /// Slack-requested disconnect envelope.
    Disconnect,
    /// Decoded but unsupported envelope type.
    Unknown,
}

impl EnvelopeClass {
    /// Return the approved stable trace spelling.
    fn as_str(self) -> &'static str {
        match self {
            Self::Malformed => "malformed",
            Self::EventsApi => "events_api",
            Self::Hello => "hello",
            Self::Disconnect => "disconnect",
            Self::Unknown => "unknown",
        }
    }
}

impl DecodedSlackEvent {
    /// Return the bounded event class used by payload-free latency traces.
    fn event_class(&self) -> &'static str {
        match self {
            Self::Message(message) if message_is_local_command(message) => "local_command",
            Self::Message(_) => "create",
            Self::Reaction(_) => "reaction",
            Self::Edit(_) => "edit",
        }
    }
}

/// Classify command-shaped messages without logging or retaining their content.
fn message_is_local_command(message: &SlackMessage) -> bool {
    let text = message.text.trim();
    let command_text = if text.starts_with("<@") {
        text.find('>')
            .map_or(text, |mention_end| text[mention_end + 1..].trim())
    } else if message.channel_type.as_deref() == Some("im") {
        text
    } else {
        return false;
    };
    command_text.is_empty() || parse_command(command_text).0.is_some()
}

/// Private monotonic timing and lifecycle authority for one accepted
/// occurrence.
struct AdmissionWork {
    /// Decoded payload retained only inside the bounded in-memory FIFO.
    event: DecodedSlackEvent,
    /// Local websocket receive instant.
    received_at: Instant,
    /// Instant immediately after successful ACK when FIFO work became runnable.
    enqueued_at: Instant,
    /// Process-local occurrence ordinal used only for TRACE correlation.
    trace_seq: u64,
    /// Socket connection generation on which this occurrence arrived.
    connection_generation: u64,
    /// Admission lifecycle epoch captured before ACK.
    ingress_epoch: u64,
    /// Immutable configuration generation captured before ACK.
    config_generation: u64,
    /// Agent-registration generation captured before ACK.
    agent_generation: u64,
    /// Exact authenticated installation team captured before ACK.
    installation_team_id: String,
    /// Queue depth bucket observed while reserving the slot.
    queue_depth_bucket: QueueDepthBucket,
}

/// Monotonic private context captured immediately after one websocket read.
#[derive(Clone, Copy)]
struct SocketFrameTiming {
    /// Socket generation local to this extension process.
    connection_generation: u64,
    /// Frame ordinal local to this extension process.
    trace_seq: u64,
    /// Instant at which `ws.next()` returned the frame.
    received_at: Instant,
}

/// Private dynamic DM learned through allowlisted `start`.
#[derive(Clone)]
struct LinkedConversation {
    /// Allowlisted Slack user id that established this DM link.
    user_id: String,
}

/// Source-bound ingress awaiting its durable harness result.
struct PendingIngress {
    /// Agent targeted by this occurrence.
    agent_id: AgentId,
    /// Authorized native route submitted to the harness.
    conversation: SlackConversation,
    /// Verified Slack account that authored the occurrence.
    user_id: String,
    /// Exact installation team which admitted the event wrapper.
    installation_team_id: String,
    /// Sender policy retained for the eventual source-bound reply route.
    policy_status: SenderPolicyStatus,
    /// Exact native occurrence identity submitted for canonical correlation.
    external_identity: ExternalMessageIdentity,
    /// Native create identity to bind after durable commit, when applicable.
    original_key: Option<PostedMessageKey>,
    /// Exact native item eligible for reaction after commit, if this is
    /// create/edit.
    reaction_message_ts: Option<String>,
    /// Monotonic extension instant at RPC submission.
    submitted_at: Instant,
    /// Payload-free process-local correlation retained until the RPC result.
    latency_trace: Option<LatencyTrace>,
}

/// Opaque canonical route returned only after durable ingress commit.
#[derive(Clone)]
struct ReplyRoute {
    /// Agent allowed to use this route.
    agent_id: AgentId,
    /// Private native destination never accepted from tool arguments.
    conversation: SlackConversation,
    /// Verified Slack account bound to the original route.
    user_id: String,
    /// First-canonical UI-only display snapshot.
    display_name: Option<String>,
    /// First-canonical operator alias snapshot.
    identity_alias: Option<String>,
    /// Installation team bound to this source occurrence.
    installation_team_id: String,
    /// Policy of the sender that activated this route.
    policy_status: SenderPolicyStatus,
}

/// A successful Slack post awaiting durable outgoing-fact completion.
struct PendingPostedMessage {
    /// Authenticated route used for the remote post.
    conversation: SlackConversation,
    /// Native identity returned by Slack.
    posted: PostedMessage,
    /// Agent that authored the post.
    agent_id: AgentId,
    /// Original tool invocation used to fail closed if completion is rejected.
    invoke: ToolStarted,
    /// Opaque model-facing reference activated only after durable completion.
    message_ref: String,
    /// Authority that must remain current for later reactions.
    authority: ReactionAuthority,
    /// Exact lifecycle/configuration authority captured before Slack I/O.
    send_authority: FrozenSendAuthority,
}

/// Live authority retained for one reaction target.
#[derive(Clone, Eq, PartialEq)]
enum ReactionAuthority {
    /// Exact committed incoming source route.
    Source {
        /// Canonical Tau message occurrence id.
        message_id: MessageId,
        /// Verified source user bound to the route.
        user_id: String,
    },
    /// Exact operator-configured proactive destination.
    ConfiguredDestination {
        /// Stable model-facing destination alias.
        alias: String,
    },
}

/// One exact Slack item addressable only through a Tau-issued reference.
#[derive(Clone, Eq, PartialEq)]
struct ReactionTarget {
    /// Agent that received or authored the message.
    agent_id: AgentId,
    /// Exact authenticated conversation route.
    conversation: SlackConversation,
    /// Exact item timestamp, which may be a thread child.
    message_ts: String,
    /// Exact installation team that minted this private authority.
    installation_team_id: String,
    /// Live route authority revalidated on every use.
    authority: ReactionAuthority,
}

/// Private semantic identity shared by refs naming the same reaction.
#[derive(Clone, Eq, Hash, PartialEq)]
struct ReactionKey {
    /// Native conversation identity.
    channel_id: String,
    /// Native exact message timestamp.
    message_ts: String,
    /// Strict canonical emoji spelling.
    emoji: String,
}

/// Local ownership for one unambiguously added reaction.
struct ReactionOwner {
    /// Agent allowed to remove the reaction.
    agent_id: AgentId,
    /// Reference pinned while ownership remains live.
    message_ref: String,
}

/// One exact in-flight reservation protected against late completion races.
struct ReactionReservation {
    /// Agent whose call owns the reservation.
    agent_id: AgentId,
    /// Monotonic token unique within this extension process.
    token: u64,
    /// Target reference pinned until the call finishes.
    message_ref: String,
    /// Whether this is an unowned add counted against ownership capacity.
    unowned_add: bool,
}

/// Terminal disposition retained for same-process tool-call replay.
#[derive(Clone)]
enum ReactionAttemptDisposition {
    /// Authorized call is currently awaiting Slack.
    InFlight,
    /// Structured successful result.
    Success(CborValue),
    /// Stable bounded error.
    Error(String),
}

/// Fingerprint and terminal result for one reaction call.
#[derive(Clone)]
struct ReactionAttempt {
    /// Exact calling agent.
    agent_id: AgentId,
    /// Exact invocation arguments.
    arguments: CborValue,
    /// Terminal result returned without repeating Slack I/O.
    disposition: ReactionAttemptDisposition,
}

/// Commit-confirmed incoming Slack create eligible for later edit references.
#[derive(Clone, Eq, PartialEq)]
struct IncomingMessageOwner {
    /// Agent that received the original create.
    agent_id: AgentId,
    /// Canonical id of the original immutable create occurrence.
    message_id: MessageId,
    /// Exact source-bound conversation and thread.
    conversation: SlackConversation,
    /// Verified original sender account.
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
    /// Immutable harness-configured extension instance for canonical matching.
    instance_name: Option<tau_proto::ExtensionName>,
    config: Option<RuntimeConfig>,
    registered_agents: HashSet<AgentId>,
    agent_labels: HashMap<AgentId, String>,
    /// Selected agent independently owned by each static route or dynamic DM.
    selected_agent_by_route: HashMap<SelectionRouteKey, AgentId>,
    /// Ingress requests waiting for a commit-gated result.
    pending_ingress: HashMap<String, PendingIngress>,
    /// Canonical opaque ids mapped to private Slack routes.
    reply_routes: HashMap<MessageId, ReplyRoute>,
    /// Oldest-first bound for canonical reply routes.
    reply_route_order: VecDeque<MessageId>,
    /// Remote posts waiting for outgoing fact plus terminal result commit.
    pending_posts: HashMap<String, PendingPostedMessage>,
    /// Exact send reservations whose acknowledged completion output is either
    /// queued behind the shared cap or owned by one active worker.
    ///
    /// An owner remains here continuously while moving from the pending FIFO to
    /// an active slot, so every replay coalesces until that exact owner
    /// releases.
    completion_resubmitting: HashSet<SendQueueReservation>,
    /// Oldest-first acknowledged completion outputs queued behind the shared
    /// worker cap. Moving the front item to an active slot is one locked state
    /// transition performed by `reserve_pending_completion_output`.
    pending_completion_outputs: VecDeque<CompletionOutput>,
    /// Bounded, non-evicting process/session ledger preventing replay reposts.
    send_ledger: HashMap<tau_proto::ToolCallId, SendLedgerEntry>,
    /// Monotonic send reservation token source.
    next_send_reservation: u64,
    /// Per-agent send lifecycle generations preventing unrelated churn from
    /// cancelling other agents.
    send_agent_generations: HashMap<AgentId, u64>,
    /// Shared slots reserved by delivery/retry workers and queued-to-running
    /// completion-output workers. Every release decrements exactly its own slot
    /// and reserves at most one FIFO successor while holding the state lock.
    active_send_workers: usize,
    /// Harness session generation captured by accepted sends.
    session_generation: u64,
    /// Typed transport capability generation captured by accepted sends.
    capability_generation: u64,
    /// Live per-channel pacing barrier consulted immediately before attempts.
    channel_attempt_deadlines: HashMap<String, Instant>,
    /// Logical-call FIFO per native channel. The front call retains its turn
    /// through its sole retry and provider backoff.
    channel_send_queues: HashMap<String, VecDeque<SendQueueReservation>>,
    /// Recent commit-confirmed incoming creates by native Slack identity.
    incoming_messages: HashMap<PostedMessageKey, IncomingMessageOwner>,
    /// Oldest-first bound for incoming create identities.
    incoming_message_order: VecDeque<PostedMessageKey>,
    /// Monotonic id source for extension-owned RPC correlation ids.
    next_route_id: u64,
    /// Whether the harness accepted this connection's Slack/reply-tool
    /// capability.
    capability_active: bool,
    /// Session-generation registration request awaiting its result.
    pending_capability_request: Option<String>,
    /// Monotonic capability request correlation source.
    next_capability_request: u64,
    /// Bounded exact D-conversation to U/W-user dynamic links.
    linked_dms: HashMap<String, LinkedConversation>,
    /// Monotonic latch preventing configuration changes after remote effects.
    config_frozen: bool,
    /// Version used to reject a stale successful worker preflight race.
    config_generation: u64,
    /// Lifecycle epoch invalidating accepted work after authority teardown.
    ingress_epoch: u64,
    /// Generation invalidating late bridge-local effects after agent changes.
    agent_generation: u64,
    /// Whether a live harness session may accept new Socket Mode occurrences.
    session_active: bool,
    worker_started: bool,
    worker_online: bool,
    worker_startup_failure_reported: bool,
    /// One-shot categorical restart notice after installation poisoning.
    installation_restart_notice_reported: bool,
    /// Whether the current consecutive verified-human API failure episode was
    /// reported.
    identity_failure_reported: bool,
    bot_user_id: Option<String>,
    /// Installing workspace paired with `bot_user_id`.
    installation_team_id: Option<String>,
    /// Process-lifetime fail-closed latch after an authenticated pair mismatch.
    installation_mismatch: bool,
    duplicate_events: DuplicateCache,
    /// Recent bridge-authored Slack posts and their owning agents.
    posted_messages: PostedMessageCache,
    /// Opaque commit-accepted refs mapped to private exact targets.
    reaction_targets: HashMap<String, ReactionTarget>,
    /// Oldest-first target insertion order.
    reaction_target_order: VecDeque<String>,
    /// Locally owned bot reactions.
    reaction_owners: HashMap<ReactionKey, ReactionOwner>,
    /// Reaction tuples reserved during Slack I/O.
    reaction_in_flight: HashMap<ReactionKey, ReactionReservation>,
    /// Monotonic token preventing late calls from clearing newer reservations.
    next_reaction_reservation: u64,
    /// Lifecycle epoch preventing late calls from mutating restored state.
    reaction_epoch: u64,
    /// Same-process terminal reaction attempts.
    reaction_attempts: HashMap<tau_proto::ToolCallId, ReactionAttempt>,
    /// Oldest-first attempt insertion order.
    reaction_attempt_order: VecDeque<tau_proto::ToolCallId>,
}

impl State {
    /// Irreversibly revoke installation-scoped authority for this process.
    fn latch_installation_mismatch(&mut self) {
        self.ingress_epoch = self.ingress_epoch.wrapping_add(1);
        self.capability_active = false;
        self.capability_generation = self.capability_generation.wrapping_add(1);
        self.pending_capability_request = None;
        self.installation_mismatch = true;
        self.pending_ingress.clear();
        self.clear_reaction_state();
        self.clear_reply_routes();
        self.clear_incoming_messages();
        self.posted_messages.clear();
        self.linked_dms.clear();
        self.selected_agent_by_route.clear();
        self.duplicate_events = DuplicateCache::default();
    }

    /// Install the first authenticated pair or require an exact reconnect
    /// match.
    ///
    /// A mismatch revokes all installation-scoped authority and requires a
    /// process restart; it never reinterprets old native routes under a new
    /// pair.
    fn install_or_match_installation(
        &mut self,
        bot_user_id: String,
        team_id: String,
    ) -> Result<bool, String> {
        if self.installation_mismatch {
            return Err(
                "Slack installation identity changed; restart Tau before reconnecting".to_owned(),
            );
        }
        match (&self.bot_user_id, &self.installation_team_id) {
            (None, None) => {
                self.bot_user_id = Some(bot_user_id);
                self.installation_team_id = Some(team_id);
                Ok(true)
            }
            (Some(current_bot), Some(current_team))
                if current_bot == &bot_user_id && current_team == &team_id =>
            {
                Ok(false)
            }
            _ => {
                self.latch_installation_mismatch();
                Err(
                    "Slack installation identity changed; restart Tau before reconnecting"
                        .to_owned(),
                )
            }
        }
    }

    /// Insert a target, evicting only the oldest target without live ownership.
    fn insert_reaction_target(&mut self, message_ref: String, target: ReactionTarget) -> bool {
        if let std::collections::hash_map::Entry::Occupied(mut entry) =
            self.reaction_targets.entry(message_ref.clone())
        {
            if entry.get() == &target {
                entry.insert(target);
                return true;
            }
            return false;
        }
        while self.reaction_targets.len() >= REACTION_TARGET_LIMIT {
            let Some(index) = self.reaction_target_order.iter().position(|candidate| {
                !self
                    .reaction_owners
                    .values()
                    .any(|owner| &owner.message_ref == candidate)
                    && !self
                        .reaction_in_flight
                        .values()
                        .any(|reservation| &reservation.message_ref == candidate)
            }) else {
                return false;
            };
            if let Some(evicted) = self.reaction_target_order.remove(index) {
                self.reaction_targets.remove(&evicted);
            }
        }
        self.reaction_target_order.push_back(message_ref.clone());
        self.reaction_targets.insert(message_ref, target);
        true
    }

    /// Store one bounded terminal reaction attempt.
    fn remember_reaction_attempt(
        &mut self,
        invoke: &ToolStarted,
        disposition: ReactionAttemptDisposition,
    ) -> bool {
        if let Some(existing) = self.reaction_attempts.get_mut(&invoke.call_id) {
            if existing.agent_id != invoke.agent_id || existing.arguments != invoke.arguments {
                return false;
            }
            existing.disposition = disposition;
            return true;
        }
        while self.reaction_attempts.len() >= REACTION_ATTEMPT_LIMIT {
            let Some(index) = self.reaction_attempt_order.iter().position(|call_id| {
                self.reaction_attempts.get(call_id).is_some_and(|attempt| {
                    !matches!(attempt.disposition, ReactionAttemptDisposition::InFlight)
                })
            }) else {
                return false;
            };
            if let Some(evicted) = self.reaction_attempt_order.remove(index) {
                self.reaction_attempts.remove(&evicted);
            }
        }
        self.reaction_attempt_order
            .retain(|call_id| call_id != &invoke.call_id);
        self.reaction_attempt_order
            .push_back(invoke.call_id.clone());
        self.reaction_attempts.insert(
            invoke.call_id.clone(),
            ReactionAttempt {
                agent_id: invoke.agent_id.clone(),
                arguments: invoke.arguments.clone(),
                disposition,
            },
        );
        true
    }

    /// Clear all reaction target, ownership, in-flight, and replay state.
    fn clear_reaction_state(&mut self) {
        self.reaction_epoch = self.reaction_epoch.wrapping_add(1);
        self.reaction_targets.clear();
        self.reaction_target_order.clear();
        self.reaction_owners.clear();
        self.reaction_in_flight.clear();
        self.reaction_attempts.clear();
        self.reaction_attempt_order.clear();
    }

    /// Remove all reaction state belonging to one unloaded agent.
    fn remove_agent_reaction_state(&mut self, agent_id: &AgentId) {
        self.reaction_targets
            .retain(|_, target| &target.agent_id != agent_id);
        self.reaction_target_order
            .retain(|message_ref| self.reaction_targets.contains_key(message_ref));
        self.reaction_owners
            .retain(|_, owner| &owner.agent_id != agent_id);
        self.reaction_attempts
            .retain(|_, attempt| &attempt.agent_id != agent_id);
        self.reaction_attempt_order
            .retain(|call_id| self.reaction_attempts.contains_key(call_id));
        self.reaction_in_flight
            .retain(|_, reservation| &reservation.agent_id != agent_id);
    }

    /// Revoke source-authorized targets for an unregistered agent, preserving
    /// proactive targets.
    fn remove_agent_source_reaction_state(&mut self, agent_id: &AgentId) {
        let revoked = self
            .reaction_targets
            .iter()
            .filter(|(_, target)| {
                &target.agent_id == agent_id
                    && matches!(target.authority, ReactionAuthority::Source { .. })
            })
            .map(|(message_ref, _)| message_ref.clone())
            .collect::<HashSet<_>>();
        self.reaction_targets
            .retain(|message_ref, _| !revoked.contains(message_ref));
        self.reaction_target_order
            .retain(|message_ref| !revoked.contains(message_ref));
        self.reaction_owners
            .retain(|_, owner| !revoked.contains(&owner.message_ref));
        self.reaction_in_flight
            .retain(|_, reservation| !revoked.contains(&reservation.message_ref));
        // Attempt fingerprints survive unregister so late calls terminalize
        // safely and same-call replay cannot regain Slack I/O authority.
    }

    /// Return whether live reaction ownership pins this source reply route.
    fn reply_route_is_pinned(&self, message_id: &MessageId) -> bool {
        self.reaction_owners
            .values()
            .map(|owner| owner.message_ref.as_str())
            .chain(
                self.reaction_in_flight
                    .values()
                    .map(|reservation| reservation.message_ref.as_str()),
            )
            .any(|message_ref| {
                self.reaction_targets
                    .get(message_ref)
                    .is_some_and(|target| {
                        matches!(
                            &target.authority,
                            ReactionAuthority::Source {
                                message_id: owned_id,
                                ..
                            } if owned_id == message_id
                        )
                    })
            })
    }

    /// Insert or refresh one canonical route while evicting the oldest route.
    fn insert_reply_route(&mut self, message_id: MessageId, route: ReplyRoute) {
        self.reply_route_order.retain(|id| id != &message_id);
        self.reply_route_order.push_back(message_id.clone());
        self.reply_routes.insert(message_id, route);
        while self.reply_routes.len() > REPLY_ROUTE_LIMIT {
            let Some(index) = self
                .reply_route_order
                .iter()
                .position(|candidate| !self.reply_route_is_pinned(candidate))
            else {
                break;
            };
            if let Some(oldest) = self.reply_route_order.remove(index) {
                self.reply_routes.remove(&oldest);
            }
        }
    }

    /// Remove all canonical routes owned by one agent.
    fn remove_agent_reply_routes(&mut self, agent_id: &AgentId) {
        self.reply_routes
            .retain(|_, route| &route.agent_id != agent_id);
        self.reply_route_order
            .retain(|id| self.reply_routes.contains_key(id));
    }

    /// Clear all private canonical routes.
    fn clear_reply_routes(&mut self) {
        self.reply_routes.clear();
        self.reply_route_order.clear();
    }

    /// Remember one committed incoming create for immutable edit references.
    fn insert_incoming_message(
        &mut self,
        key: PostedMessageKey,
        owner: IncomingMessageOwner,
    ) -> bool {
        if let Some(existing) = self.incoming_messages.get(&key) {
            return existing == &owner;
        }
        self.incoming_message_order
            .retain(|existing| existing != &key);
        self.incoming_message_order.push_back(key.clone());
        self.incoming_messages.insert(key, owner);
        while self.incoming_messages.len() > REPLY_ROUTE_LIMIT {
            if let Some(oldest) = self.incoming_message_order.pop_front() {
                self.incoming_messages.remove(&oldest);
            }
        }
        true
    }

    /// Remove all committed incoming identities owned by one agent.
    fn remove_agent_incoming_messages(&mut self, agent_id: &AgentId) {
        self.incoming_messages
            .retain(|_, owner| &owner.agent_id != agent_id);
        let retained = self
            .incoming_messages
            .keys()
            .cloned()
            .collect::<HashSet<_>>();
        self.incoming_message_order
            .retain(|key| retained.contains(key));
    }

    /// Clear all committed incoming native identities.
    fn clear_incoming_messages(&mut self) {
        self.incoming_messages.clear();
        self.incoming_message_order.clear();
    }

    /// Clear the process/session send horizon after the harness retires it.
    fn clear_send_ledger(&mut self) {
        self.send_ledger.clear();
        self.completion_resubmitting.clear();
        self.pending_completion_outputs.clear();
        self.channel_attempt_deadlines.clear();
        self.channel_send_queues.clear();
    }

    /// Atomically move at most one oldest queued completion output into a
    /// reserved shared worker slot.
    fn reserve_pending_completion_output(&mut self) -> Option<CompletionOutput> {
        if self.active_send_workers >= ACTIVE_SEND_WORKER_LIMIT {
            return None;
        }
        let output = self.pending_completion_outputs.pop_front()?;
        self.active_send_workers += 1;
        Some(output)
    }

    /// Return one agent's current send lifecycle generation.
    fn send_agent_generation(&self, agent_id: &AgentId) -> u64 {
        self.send_agent_generations
            .get(agent_id)
            .copied()
            .unwrap_or(0)
    }

    /// Advance only the affected agent's send lifecycle generation.
    fn bump_send_agent_generation(&mut self, agent_id: &AgentId) {
        let generation = self.send_agent_generation(agent_id).wrapping_add(1);
        self.send_agent_generations
            .insert(agent_id.clone(), generation);
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
    /// Resolve a logical tool name through tau-client when running under the
    /// SDK.
    fn wire_tool_name(&self, local: &str) -> tau_proto::ToolName {
        match self {
            Self::Client(handle) => handle
                .tool_name_scope()
                .and_then(|scope| scope.wire_tool(local))
                .expect("Slack runtime starts only after scoped tool declarations succeed"),
            Self::Channel(_) => tau_proto::ToolName::new(local),
        }
    }
    /// Sends one protocol frame, intentionally ignoring closed-writer failures.
    ///
    /// Slack Socket Mode workers and tool output are best-effort once the
    /// harness has disconnected or the tau-client writer has shut down.
    fn send(&self, message: HarnessInputMessage) -> bool {
        match self {
            Self::Channel(tx) => tx.send(message).is_ok(),
            Self::Client(handle) => handle.send_detached(message).is_ok(),
        }
    }

    /// Write and flush one protocol frame before reporting success.
    fn send_confirmed(&self, message: HarnessInputMessage) -> bool {
        match self {
            Self::Channel(tx) => tx.send(message).is_ok(),
            Self::Client(handle) => handle.send(message).is_ok(),
        }
    }

    /// Emits one event through the harness output channel.
    fn emit(&self, event: Event) {
        let _ = self.send(HarnessInputMessage::emit(event));
    }
}

/// Injectable OS-thread boundary for acknowledged completion output.
trait CompletionOutputSpawner: Send + Sync {
    fn spawn(&self, task: Box<dyn FnOnce() + Send>) -> std::io::Result<()>;
}

struct SystemCompletionOutputSpawner;

impl CompletionOutputSpawner for SystemCompletionOutputSpawner {
    fn spawn(&self, task: Box<dyn FnOnce() + Send>) -> std::io::Result<()> {
        std::thread::Builder::new()
            .name("tau-slack-send-completion".to_owned())
            .spawn(task)
            .map(|_| ())
    }
}

struct Extension {
    state: Arc<Mutex<State>>,
    /// Serializes completion-publication admission with session retirement.
    completion_publication_gate: Arc<Mutex<()>>,
    client: Arc<dyn SlackClient>,
    output: Output,
    shutdown: Arc<ShutdownSignal>,
    /// Event-driven cancellation for delivery retry waits.
    send_wake: Arc<SendWake>,
    /// Injectable delivery scheduler used by deterministic tests.
    send_scheduler: Arc<dyn SendScheduler>,
    /// Injectable completion thread boundary used by deterministic tests.
    completion_output_spawner: Arc<dyn CompletionOutputSpawner>,
    /// Fail-closed latch set before known completion-output retirement.
    output_failed: Arc<AtomicBool>,
    /// Process-local ordinal for private TRACE correlation.
    trace_seq: AtomicU64,
}

impl Extension {
    /// Create an extension with an injected event-driven send scheduler.
    fn new_with_scheduler(
        client: Arc<dyn SlackClient>,
        output: impl Into<Output>,
        send_scheduler: Arc<dyn SendScheduler>,
    ) -> Self {
        Self::new_with_scheduler_and_completion_spawner(
            client,
            output,
            send_scheduler,
            Arc::new(SystemCompletionOutputSpawner),
        )
    }

    /// Create an extension with both background execution boundaries injected.
    fn new_with_scheduler_and_completion_spawner(
        client: Arc<dyn SlackClient>,
        output: impl Into<Output>,
        send_scheduler: Arc<dyn SendScheduler>,
        completion_output_spawner: Arc<dyn CompletionOutputSpawner>,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            completion_publication_gate: Arc::new(Mutex::new(())),
            client,
            output: output.into(),
            shutdown: Arc::new(ShutdownSignal::new()),
            send_wake: Arc::new(SendWake::default()),
            send_scheduler,
            completion_output_spawner,
            output_failed: Arc::new(AtomicBool::new(false)),
            trace_seq: AtomicU64::new(0),
        }
    }

    /// Build the Socket Mode worker view over the primary extension's shared
    /// lifecycle state, completion-publication gate, and cancellation wake.
    fn new_socket_worker_view(
        send_retirement: SendRetirement,
        client: Arc<dyn SlackClient>,
        output: Output,
        shutdown: Arc<ShutdownSignal>,
    ) -> Self {
        let SendRetirement {
            state,
            completion_publication_gate,
            wake,
        } = send_retirement;
        Self {
            state,
            completion_publication_gate,
            client,
            output,
            shutdown,
            send_wake: wake,
            send_scheduler: Arc::new(SystemSendScheduler),
            completion_output_spawner: Arc::new(SystemCompletionOutputSpawner),
            output_failed: Arc::new(AtomicBool::new(false)),
            trace_seq: AtomicU64::new(0),
        }
    }

    /// Apply validated configuration before any successful preflight or post
    /// freezes it.
    fn apply_config(&self, cfg: RuntimeConfig) -> Result<(), String> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.config_frozen {
            return Err(immutable_config_error());
        }
        state.linked_dms.clear();
        state.config = Some(cfg);
        state.config_generation = state.config_generation.wrapping_add(1);
        state.ingress_epoch = state.ingress_epoch.wrapping_add(1);
        state.clear_send_ledger();
        state.registered_agents.clear();
        state.send_agent_generations.clear();
        state.selected_agent_by_route.clear();
        state.pending_ingress.clear();
        state.clear_reply_routes();
        state.clear_incoming_messages();
        state.pending_posts.clear();
        state.posted_messages.clear();
        state.clear_reaction_state();
        state.bot_user_id = None;
        state.installation_team_id = None;
        state.capability_active = false;
        state.capability_generation = state.capability_generation.wrapping_add(1);
        state.pending_capability_request = None;
        state.duplicate_events = DuplicateCache::default();
        self.send_wake.notify_lifecycle_change();
        Ok(())
    }

    /// Clear inactive configuration and runtime routing state after a config
    /// error.
    fn clear_config_after_error(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.config_frozen {
            return;
        }
        state.config_generation = state.config_generation.wrapping_add(1);
        state.ingress_epoch = state.ingress_epoch.wrapping_add(1);
        state.config = None;
        state.registered_agents.clear();
        state.send_agent_generations.clear();
        state.selected_agent_by_route.clear();
        state.pending_ingress.clear();
        state.clear_reply_routes();
        state.clear_incoming_messages();
        state.pending_posts.clear();
        state.clear_send_ledger();
        state.capability_active = false;
        state.capability_generation = state.capability_generation.wrapping_add(1);
        state.pending_capability_request = None;
        state.posted_messages.clear();
        state.linked_dms.clear();
        state.bot_user_id = None;
        state.installation_team_id = None;
        state.duplicate_events = DuplicateCache::default();
        state.clear_reaction_state();
        self.send_wake.notify_lifecycle_change();
    }

    /// Retire all outbound send authority at the process/session transport
    /// boundary before waking background workers.
    fn retire_send_authority(&self) {
        retire_send_state(
            &self.state,
            &self.completion_publication_gate,
            &self.send_wake,
        );
    }

    /// Remove one unloaded agent's private receive/reaction authority while
    /// retaining any Tau completion correlation for already-accepted sends.
    fn unload_agent(&self, agent_id: &AgentId) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.registered_agents.remove(agent_id);
        state.agent_generation = state.agent_generation.wrapping_add(1);
        state.bump_send_agent_generation(agent_id);
        state.agent_labels.remove(agent_id);
        state
            .selected_agent_by_route
            .retain(|_, selected| selected != agent_id);
        state.remove_agent_reply_routes(agent_id);
        state.remove_agent_incoming_messages(agent_id);
        state
            .pending_ingress
            .retain(|_, pending| &pending.agent_id != agent_id);
        state.posted_messages.remove_agent(agent_id);
        state.remove_agent_reaction_state(agent_id);
        drop(state);
        self.send_wake.notify_lifecycle_change();
    }

    /// Report whether remote activation or an authorized post froze
    /// configuration.
    fn config_frozen(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .config_frozen
    }

    /// Register the typed Slack capability for the current harness session.
    fn request_transport_capability(&self) {
        let (request_id, proactive_destinations) = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.capability_active = false;
            state.capability_generation = state.capability_generation.wrapping_add(1);
            if state.installation_mismatch {
                state.pending_capability_request = None;
                drop(state);
                self.send_wake.notify_lifecycle_change();
                return;
            }
            state.next_capability_request = state.next_capability_request.wrapping_add(1);
            let request_id = format!(
                "{CAPABILITY_REQUEST_PREFIX}{}",
                state.next_capability_request
            );
            state.pending_capability_request = Some(request_id.clone());
            let destinations = state.config.as_ref().map_or_else(Vec::new, |cfg| {
                cfg.proactive_aliases
                    .iter()
                    .filter_map(|alias| cfg.conversations.get(alias))
                    .map(send_destination_capability)
                    .collect()
            });
            (request_id, destinations)
        };
        self.output
            .send(HarnessInputMessage::RegisterTransportCapability(
                RegisterTransportCapabilityRequest {
                    request_id,
                    transport_name: TRANSPORT_NAME.to_owned(),
                    send_tool: Some(self.output.wire_tool_name(SEND_TOOL_NAME)),
                    send_destinations: proactive_destinations,
                },
            ));
        self.send_wake.notify_lifecycle_change();
    }

    /// Dispatch a Tau tool invocation owned by this extension.
    fn dispatch_scoped_tool(&self, local_tool_name: &tau_proto::ToolName, invoke: ToolStarted) {
        self.output.emit(Event::ToolProgress(ToolProgress {
            call_id: invoke.call_id.clone(),
            tool_name: invoke.tool_name.clone(),
            message: Some("slack tool started".to_owned()),
            progress: None,
            display: Some(ToolUseState {
                status: ToolUseStatus::InProgress,
                status_text: tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
                ..Default::default()
            }),
        }));
        let event = match local_tool_name.as_str() {
            REGISTER_TOOL_NAME => Some(self.handle_register(invoke)),
            CONVERSATIONS_TOOL_NAME => Some(self.handle_conversations(invoke)),
            SEND_TOOL_NAME => self.handle_send(invoke),
            REACT_TOOL_NAME => {
                if self.identical_reaction_call_in_flight(&invoke) {
                    None
                } else {
                    Some(self.handle_react(invoke))
                }
            }
            _ => Some(tool_error(invoke, "unknown slack tool".to_owned())),
        };
        if let Some(event) = event {
            self.output.emit(event);
        }
    }

    /// Coalesce an identical concurrent delivery onto its original reaction
    /// call.
    fn identical_reaction_call_in_flight(&self, invoke: &ToolStarted) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .reaction_attempts
            .get(&invoke.call_id)
            .is_some_and(|attempt| {
                attempt.agent_id == invoke.agent_id
                    && attempt.arguments == invoke.arguments
                    && matches!(attempt.disposition, ReactionAttemptDisposition::InFlight)
            })
    }

    /// Return one bounded page of static, operator-authored conversation
    /// policy.
    ///
    /// This reads only validated configuration and deliberately does not start
    /// the worker, register the caller, freeze configuration, or expose native
    /// Slack routing data.
    fn handle_conversations(&self, invoke: ToolStarted) -> Event {
        if let Err(message) = validate_object_fields(&invoke.arguments, &["limit", "cursor"]) {
            return tool_error(invoke, message);
        }
        let limit = match cbor_optional_usize_field(&invoke.arguments, "limit") {
            Ok(None) => DEFAULT_DISCOVERY_PAGE_LIMIT,
            Ok(Some(limit @ 1..=MAX_DISCOVERY_PAGE_LIMIT)) => limit,
            Ok(Some(_)) => {
                return tool_error(
                    invoke,
                    format!("`limit` must be between 1 and {MAX_DISCOVERY_PAGE_LIMIT}"),
                );
            }
            Err(message) => return tool_error(invoke, message),
        };
        let cursor = match cbor_optional_string_field(&invoke.arguments, "cursor") {
            Ok(None) => None,
            Ok(Some(cursor)) if cursor.len() <= MAX_DISCOVERY_CURSOR_BYTES => {
                match decode_discovery_cursor(&cursor) {
                    Ok(alias) => Some(alias),
                    Err(message) => return tool_error(invoke, message),
                }
            }
            Ok(Some(_)) => {
                return tool_error(
                    invoke,
                    format!("`cursor` exceeds {MAX_DISCOVERY_CURSOR_BYTES} bytes"),
                );
            }
            Err(message) => return tool_error(invoke, message),
        };
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let Some(cfg) = state.config.as_ref() else {
            return tool_error(invoke, "slack extension is not configured".to_owned());
        };
        if cursor
            .as_ref()
            .is_some_and(|alias| !cfg.conversations.contains_key(alias))
        {
            return tool_error(invoke, "`cursor` is malformed or stale".to_owned());
        }
        let mut policies = cfg
            .conversations
            .iter()
            .filter(|(alias, _)| cursor.as_ref().is_none_or(|cursor| *alias > cursor));
        let selected = policies.by_ref().take(limit).collect::<Vec<_>>();
        let mut has_more = policies.next().is_some();
        let mut page = selected
            .into_iter()
            .map(|(alias, policy)| {
                (
                    alias.as_str(),
                    conversation_policy_value(policy, cfg.proactive_aliases.contains(alias)),
                )
            })
            .collect::<Vec<_>>();
        loop {
            let next_cursor = has_more
                .then(|| encode_discovery_cursor(page.last().expect("nonempty bounded page").0));
            let mut result = vec![(
                CborValue::Text("conversations".to_owned()),
                CborValue::Array(page.iter().map(|(_, value)| value.clone()).collect()),
            )];
            if let Some(cursor) = next_cursor {
                result.push((
                    CborValue::Text("next_cursor".to_owned()),
                    CborValue::Text(cursor),
                ));
            }
            let value = CborValue::Map(result);
            if serde_json::to_vec(&value)
                .is_ok_and(|encoded| encoded.len() <= MAX_DISCOVERY_RESULT_BYTES)
            {
                return structured_tool_result(invoke, value);
            }
            page.pop();
            has_more = true;
        }
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
            if !self
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .capability_active
            {
                return tool_error(
                    invoke,
                    "Slack typed transport capability is not active; check harness diagnostics"
                        .to_owned(),
                );
            }
            let startup = {
                let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if state.worker_started {
                    None
                } else {
                    let Some(cfg) = state.config.clone() else {
                        return tool_error(invoke, "slack extension is not configured".to_owned());
                    };
                    let generation = state.config_generation;
                    drop(state);
                    match self.prepare_worker_start(&cfg) {
                        Ok(startup) => Some((cfg, startup, generation)),
                        Err(message) => return tool_error(invoke, message),
                    }
                }
            };
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((_, _, generation)) = &startup
                && state.config_generation != *generation
            {
                return tool_error(
                    invoke,
                    "Slack configuration changed during Socket Mode preflight; retry registration"
                        .to_owned(),
                );
            }
            if let Some((cfg, startup, _)) = startup
                && let Err(message) = self.start_worker_locked(&mut state, cfg, Some(startup))
            {
                return tool_error(invoke, message);
            }
            if state.registered_agents.insert(invoke.agent_id.clone()) {
                state.agent_generation = state.agent_generation.wrapping_add(1);
                state.bump_send_agent_generation(&invoke.agent_id);
            }
            state
                .agent_labels
                .entry(invoke.agent_id.clone())
                .or_insert_with(|| invoke.agent_id.to_string());
            self.send_wake.notify_lifecycle_change();
        } else {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.registered_agents.remove(&invoke.agent_id) {
                state.agent_generation = state.agent_generation.wrapping_add(1);
                state.bump_send_agent_generation(&invoke.agent_id);
            }
            state
                .selected_agent_by_route
                .retain(|_, agent| agent != &invoke.agent_id);
            state.remove_agent_reply_routes(&invoke.agent_id);
            state.remove_agent_source_reaction_state(&invoke.agent_id);
            state.remove_agent_incoming_messages(&invoke.agent_id);
            state
                .pending_ingress
                .retain(|_, pending| pending.agent_id != invoke.agent_id);
            state.posted_messages.remove_agent(&invoke.agent_id);
            self.send_wake.notify_lifecycle_change();
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
        let installation = self.authenticated_installation(cfg)?;
        self.match_established_installation(&installation)?;
        let socket_url = self
            .client
            .open_socket(cfg)
            .map_err(|error| error.to_string())?;
        validate_socket_url(&socket_url)?;
        Ok(WorkerStartup {
            bot_user_id: installation.bot_user_id,
            installation_team_id: installation.team_id,
            socket_url,
        })
    }

    /// Authenticate and validate one complete installation observation.
    ///
    /// Once a pair exists, malformed or incomplete replacement evidence is as
    /// terminal as an explicit mismatch and poisons capability until restart.
    fn authenticated_installation(
        &self,
        cfg: &RuntimeConfig,
    ) -> Result<SlackInstallationIdentity, String> {
        let raw = match self.client.auth_test(cfg) {
            Ok(raw) => raw,
            Err(error) => {
                if error == SlackApiError::MalformedResponse {
                    self.latch_invalid_installation_if_established();
                }
                return Err(error.to_string());
            }
        };
        let validated = validate_user_id("auth.test user_id", &raw.bot_user_id).and_then(|bot| {
            validate_team_id("auth.test team_id", &raw.team_id).map(|team| {
                SlackInstallationIdentity {
                    bot_user_id: bot,
                    team_id: team,
                }
            })
        });
        if validated.is_err() {
            self.latch_invalid_installation_if_established();
        }
        validated
    }

    /// Compare every complete observation with an already established pair
    /// before any subsequent Slack API call.
    fn match_established_installation(
        &self,
        installation: &SlackInstallationIdentity,
    ) -> Result<(), String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.bot_user_id.is_none() && state.installation_team_id.is_none() {
            return Ok(());
        }
        state
            .install_or_match_installation(
                installation.bot_user_id.clone(),
                installation.team_id.clone(),
            )
            .map(|_| ())
    }

    /// Poison an established pair after malformed reconnect identity evidence.
    fn latch_invalid_installation_if_established(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.bot_user_id.is_some() || state.installation_team_id.is_some() {
            state.latch_installation_mismatch();
            drop(state);
            self.send_wake.notify_lifecycle_change();
        }
    }

    fn start_worker_locked(
        &self,
        state: &mut State,
        cfg: RuntimeConfig,
        startup: Option<WorkerStartup>,
    ) -> Result<(), String> {
        if state.worker_started {
            return Ok(());
        }
        state.config_frozen = true;
        if let Some(startup) = &startup
            && let Err(error) = state.install_or_match_installation(
                startup.bot_user_id.clone(),
                startup.installation_team_id.clone(),
            )
        {
            self.send_wake.notify_lifecycle_change();
            return Err(error);
        }
        state.worker_started = true;
        state.worker_startup_failure_reported = false;
        let send_retirement = SendRetirement {
            state: Arc::clone(&self.state),
            completion_publication_gate: Arc::clone(&self.completion_publication_gate),
            wake: Arc::clone(&self.send_wake),
        };
        let output = self.output.clone();
        let client = Arc::clone(&self.client);
        let shutdown = Arc::clone(&self.shutdown);
        std::thread::spawn(move || {
            socket_worker_loop(send_retirement, client, output, cfg, startup, shutdown)
        });
        Ok(())
    }

    fn report_worker_startup_failure_once(&self, _cfg: &RuntimeConfig, message: &str) {
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
                bounded_text(message, 128)
            );
            self.output.emit(Event::HarnessNotice(HarnessNotice {
                kind: tau_proto::notice_kind::EXTENSION_NOTICE.to_owned(),
                message: bounded_text(&message, MAX_DIAGNOSTIC_BYTES),
                level: NoticeLevel::Warning,
                always_show: false,
            }));
        }
    }

    /// Mark the Socket worker offline and report a terminal installation
    /// poison.
    fn report_installation_restart_once(&self) {
        let should_report = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.worker_online = false;
            state.installation_mismatch
                && !std::mem::replace(&mut state.installation_restart_notice_reported, true)
        };
        if should_report {
            self.output.emit(Event::HarnessNotice(HarnessNotice {
                kind: tau_proto::notice_kind::EXTENSION_NOTICE.to_owned(),
                message: "Slack installation identity changed or became invalid; restart Tau before using std-slack again".to_owned(),
                level: NoticeLevel::Warning,
                always_show: true,
            }));
        }
    }

    #[cfg(test)]
    fn verified_human(&self, cfg: &RuntimeConfig, user_id: &str) -> bool {
        self.verified_human_traced(cfg, user_id, None).is_some()
    }

    /// Verify one sender while emitting only bounded, payload-free latency
    /// facts.
    fn verified_human_traced(
        &self,
        cfg: &RuntimeConfig,
        user_id: &str,
        admission: Option<&AdmissionContext>,
    ) -> Option<VerifiedSlackHuman> {
        let trace = admission.map(|context| context.trace);
        if let Some(trace) = trace {
            tracing::trace!(
                target: LOG_TARGET,
                schema = LATENCY_SCHEMA,
                connection_generation = trace.connection_generation,
                trace_seq = trace.trace_seq,
                event_class = trace.event_class,
                policy_class = if cfg.allowed_user_ids.contains(user_id) { "allowlisted" } else { "lax_permitted" },
                cache_state = "disabled",
                rate_circuit = "closed",
                "slack.identity.verification_started"
            );
        }
        let started_at = Instant::now();
        let result = self.client.verified_human_identity(cfg, user_id);
        if let Some(admission) = admission {
            admission.identity_us.set(elapsed_us(started_at));
        }
        if let Some(trace) = trace {
            let outcome = match &result {
                Ok(Some(_)) => "human",
                Ok(None) => "not_human",
                Err(error) => match error {
                    SlackApiError::RateLimited => "rate_limited",
                    SlackApiError::TransportTimeout => "timeout",
                    SlackApiError::TransportConnect => "connect",
                    SlackApiError::TransportTls => "tls",
                    SlackApiError::Transport => "transport",
                    SlackApiError::MalformedResponse => "malformed",
                    SlackApiError::Authentication
                    | SlackApiError::MissingScope
                    | SlackApiError::TargetUnavailable
                    | SlackApiError::PermissionDenied
                    | SlackApiError::InvalidRequest
                    | SlackApiError::RemoteFailure => "api_error",
                },
            };
            tracing::trace!(
                target: LOG_TARGET,
                schema = LATENCY_SCHEMA,
                connection_generation = trace.connection_generation,
                trace_seq = trace.trace_seq,
                event_class = trace.event_class,
                source = "api",
                duration_us = elapsed_us(started_at),
                outcome,
                "slack.identity.verification_finished"
            );
        }
        if !self.admission_authority_is_current(admission) {
            if let Some(admission) = admission {
                admission.mark("stale_epoch");
            }
            log_ingress_rejection("stale_epoch");
            return None;
        }
        match result {
            Ok(identity) => {
                self.state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .identity_failure_reported = false;
                if identity.is_none() {
                    if let Some(admission) = admission {
                        admission.mark("rejected_identity");
                    }
                    log_ingress_rejection("sender_not_human");
                }
                identity
            }
            Err(error) => {
                if let Some(admission) = admission {
                    admission.mark("rejected_identity");
                }
                let should_report = {
                    let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    !std::mem::replace(&mut state.identity_failure_reported, true)
                };
                if should_report {
                    tracing::warn!(target: LOG_TARGET, rejection = "identity_api_failure", error = %error, "Slack ingress occurrence rejected; users.info verification degraded");
                    self.output.emit(Event::HarnessNotice(HarnessNotice {
                        kind: tau_proto::notice_kind::EXTENSION_NOTICE.to_owned(),
                        message: bounded_text(
                            &format!(
                                "Slack rejected one ingress occurrence because users.info verification failed (check users:read scope and app reinstall): {}",
                                error
                            ),
                            MAX_DIAGNOSTIC_BYTES,
                        ),
                        level: NoticeLevel::Warning,
                        always_show: false,
                    }));
                }
                None
            }
        }
    }

    /// Execute one separately authorized, exact-reference Slack reaction call.
    fn handle_react(&self, invoke: ToolStarted) -> Event {
        {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if let Some(attempt) = state.reaction_attempts.get(&invoke.call_id) {
                if attempt.agent_id != invoke.agent_id || attempt.arguments != invoke.arguments {
                    return tool_error(
                        invoke,
                        "slack_react call id was replayed with conflicting arguments".to_owned(),
                    );
                }
                return match &attempt.disposition {
                    ReactionAttemptDisposition::InFlight => reaction_coalesced(invoke),
                    ReactionAttemptDisposition::Success(result) => {
                        structured_tool_result(invoke, result.clone())
                    }
                    ReactionAttemptDisposition::Error(message) => {
                        tool_error(invoke, message.clone())
                    }
                };
            }
        }
        let parsed = (|| {
            validate_object_fields(&invoke.arguments, &["message_ref", "emoji", "action"])?;
            let message_ref = cbor_string_field(&invoke.arguments, "message_ref")?;
            let emoji = cbor_string_field(&invoke.arguments, "emoji")?;
            let action_text = cbor_string_field(&invoke.arguments, "action")?;
            if message_ref.is_empty() || message_ref.len() > 128 {
                return Err("`message_ref` must contain 1 to 128 bytes".to_owned());
            }
            if !valid_outbound_emoji(&emoji) {
                return Err("`emoji` must be a valid lowercase Slack emoji name".to_owned());
            }
            let action = ReactionActionKind::parse(&action_text)
                .ok_or_else(|| "`action` must be `add` or `remove`".to_owned())?;
            Ok((message_ref, emoji, action))
        })();
        let (message_ref, emoji, action) = match parsed {
            Ok(parsed) => parsed,
            Err(message) => return self.finish_reaction_error(invoke, message),
        };
        let prepared = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let Some(cfg) = state.config.clone() else {
                return self.finish_reaction_error_locked(
                    &mut state,
                    invoke,
                    "Slack message reference is unknown, stale, or unauthorized".to_owned(),
                );
            };
            let Some(target) = state.reaction_targets.get(&message_ref).cloned() else {
                return self.finish_reaction_error_locked(
                    &mut state,
                    invoke,
                    "Slack message reference is unknown, stale, or unauthorized".to_owned(),
                );
            };
            if target.agent_id != invoke.agent_id
                || !state.capability_active
                || !reaction_target_authorized(&state, &cfg, &target)
            {
                return self.finish_reaction_error_locked(
                    &mut state,
                    invoke,
                    "Slack message reference is unknown, stale, or unauthorized".to_owned(),
                );
            }
            if let Some(attempt) = state.reaction_attempts.get(&invoke.call_id) {
                if attempt.agent_id == invoke.agent_id && attempt.arguments == invoke.arguments {
                    return reaction_coalesced(invoke);
                }
                return tool_error(
                    invoke,
                    "slack_react call id was replayed with conflicting arguments".to_owned(),
                );
            }
            if state.reaction_attempts.len() >= REACTION_ATTEMPT_LIMIT
                && state.reaction_attempts.values().all(|attempt| {
                    matches!(attempt.disposition, ReactionAttemptDisposition::InFlight)
                })
            {
                return tool_error(invoke, "Slack reaction attempt capacity is full".to_owned());
            }
            let key = ReactionKey {
                channel_id: target.conversation.channel_id.clone(),
                message_ts: target.message_ts.clone(),
                emoji: emoji.clone(),
            };
            if state.reaction_in_flight.contains_key(&key) {
                return self.finish_reaction_error_locked(
                    &mut state,
                    invoke,
                    "Slack reaction is already in progress".to_owned(),
                );
            }
            let owner_agent = state
                .reaction_owners
                .get(&key)
                .map(|owner| owner.agent_id.clone());
            let owned_before = owner_agent.as_ref() == Some(&invoke.agent_id);
            match action {
                ReactionActionKind::Add => {
                    if owner_agent
                        .as_ref()
                        .is_some_and(|owner| owner != &invoke.agent_id)
                    {
                        return self.finish_reaction_error_locked(
                            &mut state,
                            invoke,
                            "Slack reaction is owned by another agent".to_owned(),
                        );
                    }
                    if owner_agent.is_none()
                        && state.reaction_owners.len()
                            + state
                                .reaction_in_flight
                                .values()
                                .filter(|reservation| reservation.unowned_add)
                                .count()
                            >= REACTION_OWNERSHIP_LIMIT
                    {
                        return self.finish_reaction_error_locked(
                            &mut state,
                            invoke,
                            "Slack reaction ownership capacity is full".to_owned(),
                        );
                    }
                }
                ReactionActionKind::Remove => {
                    if !owned_before {
                        return self.finish_reaction_error_locked(
                            &mut state,
                            invoke,
                            "Slack reaction is not owned by this agent".to_owned(),
                        );
                    }
                }
            }
            state.next_reaction_reservation = state.next_reaction_reservation.wrapping_add(1);
            let reservation = state.next_reaction_reservation;
            state.reaction_in_flight.insert(
                key.clone(),
                ReactionReservation {
                    agent_id: invoke.agent_id.clone(),
                    token: reservation,
                    message_ref: message_ref.clone(),
                    unowned_add: action == ReactionActionKind::Add && !owned_before,
                },
            );
            debug_assert!(
                state.remember_reaction_attempt(&invoke, ReactionAttemptDisposition::InFlight)
            );
            state.config_frozen = true;
            (
                cfg,
                target,
                key,
                state.config_generation,
                state.reaction_epoch,
                reservation,
                owned_before,
            )
        };
        let (cfg, target, key, generation, epoch, reservation, owned_before) = prepared;
        let outcome = self.client.react(
            &cfg,
            action,
            &target.conversation.channel_id,
            &target.message_ts,
            &emoji,
        );
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let reservation_matches = state
            .reaction_in_flight
            .get(&key)
            .is_some_and(|current| current.token == reservation);
        if reservation_matches {
            state.reaction_in_flight.remove(&key);
        }
        let current = state.config_generation == generation
            && state.reaction_epoch == epoch
            && reservation_matches
            && state.capability_active
            && state.config.as_ref().is_some_and(|current_cfg| {
                state
                    .reaction_targets
                    .get(&message_ref)
                    .is_some_and(|current_target| {
                        current_target == &target
                            && current_target.agent_id == invoke.agent_id
                            && reaction_target_authorized(&state, current_cfg, current_target)
                    })
            });
        if !current {
            let message = "Slack message reference is unknown, stale, or unauthorized".to_owned();
            if state
                .reaction_attempts
                .get(&invoke.call_id)
                .is_some_and(|attempt| {
                    attempt.agent_id == invoke.agent_id
                        && attempt.arguments == invoke.arguments
                        && matches!(attempt.disposition, ReactionAttemptDisposition::InFlight)
                })
            {
                return self.finish_reaction_error_locked(&mut state, invoke, message);
            }
            return tool_error(invoke, message);
        }
        let success = match (action, outcome) {
            (ReactionActionKind::Add, Ok(())) if current => {
                state.reaction_owners.insert(
                    key,
                    ReactionOwner {
                        agent_id: invoke.agent_id.clone(),
                        message_ref: message_ref.clone(),
                    },
                );
                true
            }
            (ReactionActionKind::Add, Err(ReactionApiError::AlreadyReacted))
                if current && owned_before =>
            {
                true
            }
            (ReactionActionKind::Remove, Ok(()))
            | (ReactionActionKind::Remove, Err(ReactionApiError::NoReaction))
                if current && owned_before =>
            {
                state.reaction_owners.remove(&key);
                true
            }
            (_, outcome) => {
                let message = reaction_error_message(outcome.err(), action, current, owned_before);
                return self.finish_reaction_error_locked(&mut state, invoke, message);
            }
        };
        debug_assert!(success);
        let result = CborValue::Map(vec![
            example_field("status", example_text("ok")),
            example_field("action", example_text(action.as_str())),
            example_field("emoji", example_text(&emoji)),
        ]);
        if !state
            .remember_reaction_attempt(&invoke, ReactionAttemptDisposition::Success(result.clone()))
        {
            return tool_error(
                invoke,
                "slack_react call id was replayed with conflicting arguments".to_owned(),
            );
        }
        structured_tool_result(invoke, result)
    }

    /// Store and return one terminal reaction error after acquiring state.
    fn finish_reaction_error(&self, invoke: ToolStarted, message: String) -> Event {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        self.finish_reaction_error_locked(&mut state, invoke, message)
    }

    /// Store and return one terminal reaction error while state is locked.
    fn finish_reaction_error_locked(
        &self,
        state: &mut State,
        invoke: ToolStarted,
        message: String,
    ) -> Event {
        if state
            .remember_reaction_attempt(&invoke, ReactionAttemptDisposition::Error(message.clone()))
        {
            tool_error(invoke, message)
        } else {
            tool_error(
                invoke,
                "slack_react call id was replayed with conflicting arguments".to_owned(),
            )
        }
    }

    /// Install test-side post ownership using an authenticated conversation.
    #[cfg(test)]
    fn remember_posted_message(
        &self,
        conversation: SlackConversation,
        post: PostedMessage,
        agent_id: AgentId,
    ) {
        if post.thread_ts.is_some() && post.thread_ts != conversation.thread_ts {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let Some(installation_team_id) = state.installation_team_id.clone() else {
            return;
        };
        state.posted_messages.insert(
            PostedMessageKey::new(&conversation.channel_id, &post.ts),
            PostedMessageOwner {
                agent_id,
                thread_ts: conversation.thread_ts,
                installation_team_id,
            },
        );
    }

    #[cfg(test)]
    fn process_slack_reaction(&self, reaction: SlackReaction) {
        self.process_slack_reaction_admitted(reaction, None);
    }

    /// Process one reaction with explicit FIFO lifecycle and timing authority.
    fn process_slack_reaction_admitted(
        &self,
        reaction: SlackReaction,
        admission: Option<&AdmissionContext>,
    ) {
        if validate_conversation_id("reaction.channel", &reaction.channel_id).is_err()
            || validate_user_id("reaction.user", &reaction.user_id).is_err()
            || validate_reaction_name(&reaction.reaction).is_err()
            || validate_slack_ts(&reaction.message_ts).is_err()
            || reaction
                .thread_ts
                .as_deref()
                .is_some_and(|ts| validate_slack_ts(ts).is_err())
        {
            log_ingress_rejection("malformed_event");
            return;
        }
        let cfg = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let Some(cfg) = state.config.clone() else {
                return;
            };
            if cfg.sender_policy(&reaction.user_id).is_none()
                || state.bot_user_id.as_deref() == Some(reaction.user_id.as_str())
                || !conversation_has_receive_source(&state, &cfg, &reaction.channel_id)
            {
                log_ingress_rejection("reaction_policy");
                return;
            }
            cfg
        };
        let Some(identity) = self.verified_human_traced(&cfg, &reaction.user_id, admission) else {
            return;
        };
        let (cfg, agent_id, route) = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let Some(cfg) = state.config.clone() else {
                return;
            };
            if cfg.sender_policy(&reaction.user_id).is_none()
                || state.bot_user_id.as_deref() == Some(reaction.user_id.as_str())
                || !conversation_has_receive_source(&state, &cfg, &reaction.channel_id)
            {
                log_ingress_rejection("reaction_route");
                return;
            }
            let key = PostedMessageKey::new(&reaction.channel_id, &reaction.message_ts);
            let Some(owner) = state.posted_messages.get(&key) else {
                log_ingress_rejection("reaction_unknown_target");
                return;
            };
            if state.installation_team_id.as_deref() != Some(owner.installation_team_id.as_str()) {
                log_ingress_rejection("reaction_unknown_target");
                return;
            }
            if reaction.thread_ts.is_some() && reaction.thread_ts != owner.thread_ts {
                log_ingress_rejection("thread_mismatch");
                return;
            }
            if !state.registered_agents.contains(&owner.agent_id) {
                return;
            }
            let route = resolve_receive_route(
                &cfg,
                &state,
                &reaction.channel_id,
                owner.thread_ts.as_deref(),
                None,
                &reaction.user_id,
            );
            let Some(route) = route else {
                log_ingress_rejection("reaction_route");
                return;
            };
            (cfg, owner.agent_id.clone(), route)
        };
        let dedup_key = reaction.event_id.clone().unwrap_or_else(|| {
            format!(
                "reaction:{}:{}:{}:{}:{}",
                reaction.event_type.as_str(),
                reaction.channel_id,
                reaction.message_ts,
                reaction.reaction,
                reaction.user_id
            )
        });
        self.submit_ingress(
            &cfg,
            IngressSubmission {
                conversation: route,
                agent_id,
                sender: IngressSender {
                    policy_status: cfg
                        .sender_policy(&reaction.user_id)
                        .expect("sender revalidated"),
                    user_id: reaction.user_id,
                    display_name: identity.display_name,
                    identity_alias: cfg.sender_aliases.get(&identity.user_id).cloned(),
                },
                operation: MessageOperation::Reaction {
                    target: MessageRef {
                        message_id: None,
                        external_message_id: Some(reaction.message_ts.clone()),
                    },
                    action: match reaction.event_type {
                        ReactionKind::Added => ReactionAction::Add,
                        ReactionKind::Removed => ReactionAction::Remove,
                    },
                    reaction: MessageReaction {
                        name: reaction.reaction,
                        display: None,
                    },
                },
                external_identity: ExternalMessageIdentity {
                    event_id: reaction.event_id,
                    message_id: Some(reaction.message_ts),
                    revision_id: None,
                    dedup_key: Some(dedup_key),
                },
            },
            admission,
        );
    }

    /// Route a validated edit only when its original committed create is known.
    ///
    /// This commit-confirmed ownership lookup and its fail-closed rejection
    /// path implement `DESIGN-tau-ext-slack-edit-ownership`.
    #[cfg(test)]
    fn process_slack_edit(&self, edit: SlackEdit) {
        self.process_slack_edit_admitted(edit, None);
    }

    /// Process one edit with explicit FIFO lifecycle and timing authority.
    fn process_slack_edit_admitted(&self, edit: SlackEdit, admission: Option<&AdmissionContext>) {
        if validate_conversation_id("edit.channel", &edit.channel_id).is_err()
            || validate_user_id("edit.user", &edit.editor_user_id).is_err()
            || validate_slack_ts(&edit.message_ts).is_err()
            || validate_slack_ts(&edit.revision_ts).is_err()
            || edit
                .thread_ts
                .as_deref()
                .is_some_and(|thread| validate_slack_ts(thread).is_err())
        {
            log_ingress_rejection("malformed_event");
            return;
        }
        let (cfg, owner) = {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let Some(cfg) = state.config.clone() else {
                return;
            };
            if cfg.sender_policy(&edit.editor_user_id).is_none()
                || state.bot_user_id.as_deref() == Some(edit.editor_user_id.as_str())
                || !conversation_has_receive_source(&state, &cfg, &edit.channel_id)
            {
                log_ingress_rejection("edit_policy");
                return;
            }
            let key = PostedMessageKey::new(&edit.channel_id, &edit.message_ts);
            let Some(owner) = state.incoming_messages.get(&key).cloned() else {
                log_ingress_rejection("edit_unknown_target");
                return;
            };
            let thread_matches = owner.conversation.thread_ts == edit.thread_ts
                || (edit.thread_ts.is_none()
                    && owner.conversation.thread_ts.as_deref() == Some(edit.message_ts.as_str()));
            if owner.user_id != edit.editor_user_id
                || !thread_matches
                || !state.registered_agents.contains(&owner.agent_id)
            {
                log_ingress_rejection("edit_owner_or_thread");
                return;
            }
            (cfg, owner)
        };
        let Some(identity) = self.verified_human_traced(&cfg, &edit.editor_user_id, admission)
        else {
            return;
        };
        let text = edit.text.trim();
        if text.is_empty() || text.len() > cfg.max_message_bytes {
            log_ingress_rejection("malformed_text");
            return;
        }
        let dedup_key = edit.event_id.clone().unwrap_or_else(|| {
            format!(
                "edit:{}:{}:{}",
                edit.channel_id, edit.message_ts, edit.revision_ts
            )
        });
        self.submit_ingress(
            &cfg,
            IngressSubmission {
                conversation: owner.conversation,
                agent_id: owner.agent_id,
                sender: IngressSender {
                    policy_status: cfg
                        .sender_policy(&edit.editor_user_id)
                        .expect("sender admitted"),
                    user_id: edit.editor_user_id,
                    display_name: identity.display_name,
                    identity_alias: cfg.sender_aliases.get(&identity.user_id).cloned(),
                },
                operation: MessageOperation::Edit {
                    target: MessageRef {
                        message_id: Some(owner.message_id),
                        external_message_id: Some(edit.message_ts.clone()),
                    },
                    payload: MessagePayload::Text {
                        text: text.to_owned(),
                        format: TextFormat::Plain,
                    },
                },
                external_identity: ExternalMessageIdentity {
                    event_id: edit.event_id,
                    message_id: Some(edit.message_ts),
                    revision_id: Some(edit.revision_ts),
                    dedup_key: Some(dedup_key),
                },
            },
            admission,
        );
    }

    #[cfg(test)]
    fn process_slack_message(&self, message: SlackMessage) {
        self.process_slack_message_admitted(message, None);
    }

    /// Process one message with explicit FIFO lifecycle and timing authority.
    fn process_slack_message_admitted(
        &self,
        mut message: SlackMessage,
        admission: Option<&AdmissionContext>,
    ) {
        if message.bot_id.is_some() || message.subtype.is_some() {
            log_ingress_rejection(if message.bot_id.is_some() {
                "bot_message"
            } else {
                "unsupported_subtype"
            });
            return;
        }
        if validate_conversation_id("event.channel", &message.channel_id).is_err()
            || validate_user_id("event.user", &message.user_id).is_err()
            || message
                .ts
                .as_deref()
                .is_none_or(|ts| validate_slack_ts(ts).is_err())
            || message
                .thread_ts
                .as_deref()
                .is_some_and(|ts| validate_slack_ts(ts).is_err())
            || (message.event_type == "message" && message.channel_type.is_none())
        {
            log_ingress_rejection("malformed_event");
            return;
        }
        let Some(cfg) = self.config_for_allowed_message(&message) else {
            return;
        };
        if self.is_self_message(&message) {
            log_ingress_rejection("bot_self");
            return;
        }
        if !matches!(message.event_type.as_str(), "app_mention" | "message") {
            log_ingress_rejection("unsupported_event");
            return;
        }
        let is_dm = message.channel_type.as_deref() == Some("im");
        if !self.accepts_event_conversation(&cfg, &message, is_dm) {
            log_ingress_rejection("conversation_or_trigger");
            return;
        }
        if let Some(route) = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            static_receive_route_for_message(&cfg, &message).or_else(|| {
                resolve_receive_route(
                    &cfg,
                    &state,
                    &message.channel_id,
                    message.thread_ts.as_deref(),
                    message.channel_type.as_deref(),
                    &message.user_id,
                )
            })
        } {
            message.thread_ts = route.thread_ts;
        }
        let Some(identity) = self.verified_human_traced(&cfg, &message.user_id, admission) else {
            return;
        };
        let leading_mention = self.has_leading_bot_mention(&message.text);
        let Some(mut text) = self.trimmed_message_text(&cfg, &message, admission) else {
            return;
        };
        if leading_mention {
            text = self.strip_bot_mention(&text);
        }
        if text.is_empty() {
            if !self.insert_command_duplicate(&message, admission) {
                return;
            }
            self.reply(
                &cfg,
                &message.channel_id,
                message.thread_ts.as_deref(),
                help_text(),
                admission,
            );
            return;
        }
        let (command, rest) = if is_dm || leading_mention {
            parse_command(&text)
        } else {
            (None, "")
        };
        // Lax senders contribute untrusted prompt content; they never gain
        // bridge-control authority (including linking or target selection).
        if command.is_some_and(|command| {
            matches!(
                command,
                "start" | "/start" | "agents" | "/agents" | "select" | "/select" | "to" | "/to"
            ) || command.starts_with('/')
        }) && !cfg.allowed_user_ids.contains(&message.user_id)
        {
            log_ingress_rejection("sender_control_policy");
            return;
        }
        if command.is_some()
            && !matches!(command, Some("to" | "/to"))
            && !self.insert_command_duplicate(&message, admission)
        {
            log_ingress_rejection("command_dedup");
            return;
        }
        if self.rejects_unlinked_command(&cfg, &message, command, admission) {
            return;
        }
        if self.handle_command(&cfg, &message, &identity, command, rest, admission) {
            return;
        }
        self.route_plain_text(&cfg, &message, &identity, &text, admission);
    }

    /// Suppress retry side effects for bridge-local commands only.
    ///
    /// Routed occurrences deliberately bypass this cache because durable
    /// harness deduplication must observe reconnect/retry submissions.
    fn insert_command_duplicate(
        &self,
        message: &SlackMessage,
        admission: Option<&AdmissionContext>,
    ) -> bool {
        if !self.local_effect_authority_is_current(admission) {
            return false;
        }
        let key = message
            .ts
            .as_ref()
            .map(|ts| format!("local:{}:{ts}", message.channel_id))
            .or_else(|| message.event_id.clone());
        let inserted = key.is_none_or(|key| {
            self.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .duplicate_events
                .insert_new(key)
        });
        if !inserted && let Some(admission) = admission {
            admission.mark("duplicate_local");
        }
        inserted
    }

    fn config_for_allowed_message(&self, message: &SlackMessage) -> Option<RuntimeConfig> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = state.config.clone()?;
        if cfg.sender_policy(&message.user_id).is_some() {
            Some(cfg)
        } else {
            log_ingress_rejection("sender_policy");
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

    fn accepts_event_conversation(
        &self,
        cfg: &RuntimeConfig,
        message: &SlackMessage,
        is_dm: bool,
    ) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let route = static_receive_route_for_message(cfg, message).or_else(|| {
            resolve_receive_route(
                cfg,
                &state,
                &message.channel_id,
                message.thread_ts.as_deref(),
                message.channel_type.as_deref(),
                &message.user_id,
            )
        });
        let Some(route) = route else {
            // An unlinked allowlisted DM is admitted only far enough to run
            // the explicit dynamic `start` command.
            return is_dm
                && message.event_type == "message"
                && cfg.dynamic_direct_messages.is_some()
                && cfg.allowed_user_ids.contains(&message.user_id)
                && !state.linked_dms.contains_key(&message.channel_id)
                && message.channel_id.starts_with('D');
        };
        if route.kind == ConversationPolicyKind::Dm {
            return message.event_type == "message";
        }
        let receive = cfg
            .conversations
            .get(&route.alias)
            .and_then(|policy| policy.receive)
            .expect("static receive route");
        message.event_type == "app_mention"
            || (message.event_type == "message" && receive == ReceiveMode::AllMessages)
    }

    fn trimmed_message_text(
        &self,
        cfg: &RuntimeConfig,
        message: &SlackMessage,
        admission: Option<&AdmissionContext>,
    ) -> Option<String> {
        let text = message.text.trim();
        if text.is_empty() {
            if !self.insert_command_duplicate(message, admission) {
                return None;
            }
            self.reply(
                cfg,
                &message.channel_id,
                message.thread_ts.as_deref(),
                "Only text messages are supported by this Tau bridge.",
                admission,
            );
            None
        } else if text.len() > cfg.max_message_bytes {
            if !self.insert_command_duplicate(message, admission) {
                return None;
            }
            self.reply(
                cfg,
                &message.channel_id,
                message.thread_ts.as_deref(),
                "Slack message is too large for this Tau bridge.",
                admission,
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

    fn has_leading_bot_mention(&self, text: &str) -> bool {
        let bot_user_id = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .bot_user_id
            .clone();
        bot_user_id.is_some_and(|id| text.trim().starts_with(&format!("<@{id}>")))
    }

    fn rejects_unlinked_command(
        &self,
        cfg: &RuntimeConfig,
        message: &SlackMessage,
        command: Option<&str>,
        admission: Option<&AdmissionContext>,
    ) -> bool {
        let has_linked_dm = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .linked_dms
            .contains_key(&message.channel_id);
        let has_static_receive = static_receive_route_for_message(cfg, message).is_some();
        if has_static_receive || has_linked_dm || matches!(command, Some("start") | Some("/start"))
        {
            return false;
        }
        self.reply(
            cfg,
            &message.channel_id,
            message.thread_ts.as_deref(),
            "Send start in an allowlisted Slack DM before routing messages to Tau.",
            admission,
        );
        true
    }

    fn handle_command(
        &self,
        cfg: &RuntimeConfig,
        message: &SlackMessage,
        identity: &VerifiedSlackHuman,
        command: Option<&str>,
        rest: &str,
        admission: Option<&AdmissionContext>,
    ) -> bool {
        match command {
            Some("start" | "/start") => {
                self.handle_start_command(
                    cfg,
                    message,
                    message.channel_type.as_deref() == Some("im"),
                    admission,
                );
                true
            }
            Some("agents" | "/agents") => {
                self.handle_agents_command(cfg, message, admission);
                true
            }
            Some("select" | "/select") => {
                self.handle_select_command(cfg, message, rest, admission);
                true
            }
            Some("to" | "/to") => {
                self.handle_to_command(cfg, message, identity, rest, admission);
                true
            }
            Some(command) if command.starts_with('/') => {
                self.reply(
                    cfg,
                    &message.channel_id,
                    message.thread_ts.as_deref(),
                    "Unknown Slack command. Supported commands: start, agents, select, to.",
                    admission,
                );
                true
            }
            Some(_) | None => false,
        }
    }

    fn handle_start_command(
        &self,
        cfg: &RuntimeConfig,
        message: &SlackMessage,
        is_dm: bool,
        admission: Option<&AdmissionContext>,
    ) {
        if !self.admission_authority_is_current(admission) {
            return;
        }
        if !is_dm {
            self.reply(
                cfg,
                &message.channel_id,
                message.thread_ts.as_deref(),
                "Dynamic Slack linking is available only in one-to-one DMs.",
                admission,
            );
            return;
        }
        if static_parent_receive_covers_dm(cfg, &message.channel_id) {
            self.reply(
                cfg,
                &message.channel_id,
                message.thread_ts.as_deref(),
                help_text(),
                admission,
            );
            return;
        }
        if static_receive_covers_dm(cfg, &message.channel_id) {
            self.reply(
                cfg,
                &message.channel_id,
                message.thread_ts.as_deref(),
                "This DM already has a fixed-thread receive policy; dynamic linking cannot broaden it.",
                admission,
            );
            return;
        }
        if cfg.dynamic_direct_messages.is_none() {
            self.reply(
                cfg,
                &message.channel_id,
                message.thread_ts.as_deref(),
                "Dynamic Slack DMs are disabled; configure `dynamic_direct_messages` or a static DM route.",
                admission,
            );
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if admission.is_some_and(|context| !context.matches_local_state(&state))
            || self.shutdown.is_requested()
            || state.registered_agents.is_empty()
        {
            return;
        }
        if let Some(existing) = state.linked_dms.get(&message.channel_id)
            && existing.user_id != message.user_id
        {
            drop(state);
            self.reply(
                cfg,
                &message.channel_id,
                message.thread_ts.as_deref(),
                "This DM is already linked to another exact Slack user.",
                admission,
            );
            return;
        }
        if !state.linked_dms.contains_key(&message.channel_id)
            && state.linked_dms.len() >= DYNAMIC_DM_LIMIT
        {
            drop(state);
            self.reply(
                cfg,
                &message.channel_id,
                message.thread_ts.as_deref(),
                "Dynamic Slack DM link capacity reached; restart Tau or configure a static DM route.",
                admission,
            );
            return;
        }
        state.linked_dms.insert(
            message.channel_id.clone(),
            LinkedConversation {
                user_id: message.user_id.clone(),
            },
        );
        drop(state);
        self.reply(
            cfg,
            &message.channel_id,
            message.thread_ts.as_deref(),
            help_text(),
            admission,
        );
    }

    fn handle_agents_command(
        &self,
        cfg: &RuntimeConfig,
        message: &SlackMessage,
        admission: Option<&AdmissionContext>,
    ) {
        let reply = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            agents_text(&state)
        };
        self.reply(
            cfg,
            &message.channel_id,
            message.thread_ts.as_deref(),
            &reply,
            admission,
        );
    }

    fn handle_select_command(
        &self,
        cfg: &RuntimeConfig,
        message: &SlackMessage,
        rest: &str,
        admission: Option<&AdmissionContext>,
    ) {
        if !self.local_effect_authority_is_current(admission) {
            return;
        }
        if rest.trim().is_empty() {
            self.reply(
                cfg,
                &message.channel_id,
                message.thread_ts.as_deref(),
                "Usage: select <agent-id-or-prefix>",
                admission,
            );
            return;
        }
        let reply = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if admission.is_some_and(|context| !context.matches_local_state(&state))
                || self.shutdown.is_requested()
            {
                return;
            }
            match resolve_agent(&state, rest.trim()) {
                Ok(agent_id) => {
                    if let Some(route_key) = current_route_key(&state, cfg, message) {
                        state
                            .selected_agent_by_route
                            .insert(route_key, agent_id.clone());
                        format!("Selected {}", agent_designator(&state, &agent_id))
                    } else {
                        "Slack route is no longer authorized.".to_owned()
                    }
                }
                Err(reply) => reply,
            }
        };
        self.reply(
            cfg,
            &message.channel_id,
            message.thread_ts.as_deref(),
            &reply,
            admission,
        );
    }

    fn handle_to_command(
        &self,
        cfg: &RuntimeConfig,
        message: &SlackMessage,
        identity: &VerifiedSlackHuman,
        rest: &str,
        admission: Option<&AdmissionContext>,
    ) {
        let (target, body) = split_first(rest);
        if target.is_empty() || body.trim().is_empty() {
            if !self.insert_command_duplicate(message, admission) {
                return;
            }
            self.reply(
                cfg,
                &message.channel_id,
                message.thread_ts.as_deref(),
                "Usage: to <agent-id-or-prefix> <message>",
                admission,
            );
            return;
        }
        match self.resolve_registered_agent(target) {
            Ok(agent_id) => self.route_text(message, identity, agent_id, body.trim(), admission),
            Err(reply) => {
                if self.insert_command_duplicate(message, admission) {
                    self.reply(
                        cfg,
                        &message.channel_id,
                        message.thread_ts.as_deref(),
                        &reply,
                        admission,
                    );
                }
            }
        }
    }

    fn route_plain_text(
        &self,
        cfg: &RuntimeConfig,
        message: &SlackMessage,
        identity: &VerifiedSlackHuman,
        text: &str,
        admission: Option<&AdmissionContext>,
    ) {
        let route_key = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            current_route_key(&state, cfg, message)
        };
        let Some(route_key) = route_key else {
            log_ingress_rejection("route_changed");
            return;
        };
        match self.plain_text_target(&route_key) {
            Ok(agent_id) => self.route_text(message, identity, agent_id, text, admission),
            Err(reply) => {
                if self.insert_command_duplicate(message, admission) {
                    self.reply(
                        cfg,
                        &message.channel_id,
                        message.thread_ts.as_deref(),
                        &reply,
                        admission,
                    );
                }
            }
        }
    }

    fn plain_text_target(&self, route_key: &SelectionRouteKey) -> Result<AgentId, String> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(agent_id) = state.selected_agent_by_route.get(route_key)
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
                self.output.wire_tool_name(REGISTER_TOOL_NAME)
            ))
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

    fn reply(
        &self,
        cfg: &RuntimeConfig,
        channel_id: &str,
        thread_ts: Option<&str>,
        text: &str,
        admission: Option<&AdmissionContext>,
    ) {
        if self.local_effect_authority_is_current(admission) {
            self.post_message_traced(
                cfg,
                channel_id,
                text,
                thread_ts,
                admission.map(|context| context.trace),
            );
            if let Some(admission) = admission {
                admission.mark("local_effect");
            }
        }
    }

    /// Return whether explicit FIFO lifecycle authority remains live.
    fn admission_authority_is_current(&self, admission: Option<&AdmissionContext>) -> bool {
        admission.is_none_or(|context| {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            context.matches_state(&state) && !self.shutdown.is_requested()
        })
    }

    /// Return whether a FIFO occurrence may still create bridge-local effects.
    fn local_effect_authority_is_current(&self, admission: Option<&AdmissionContext>) -> bool {
        admission.is_none_or(|context| {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            context.matches_local_state(&state) && !self.shutdown.is_requested()
        })
    }

    /// Send a local reply while keeping native destination and content out of
    /// traces.
    fn post_message_traced(
        &self,
        cfg: &RuntimeConfig,
        channel_id: &str,
        text: &str,
        thread_ts: Option<&str>,
        trace: Option<LatencyTrace>,
    ) {
        if let Some(trace) = trace {
            tracing::trace!(
                target: LOG_TARGET,
                schema = LATENCY_SCHEMA,
                connection_generation = trace.connection_generation,
                trace_seq = trace.trace_seq,
                event_class = trace.event_class,
                post_class = "local_reply",
                "slack.api.post_message_started"
            );
        }
        let started_at = Instant::now();
        let mode = SlackPostMode::bridge_literal(text);
        let body = FrozenPostBody::new(channel_id, thread_ts, &mode);
        let result = self.client.post_message(cfg, &body);
        if let Some(trace) = trace {
            tracing::trace!(
                target: LOG_TARGET,
                schema = LATENCY_SCHEMA,
                connection_generation = trace.connection_generation,
                trace_seq = trace.trace_seq,
                event_class = trace.event_class,
                post_class = "local_reply",
                duration_us = elapsed_us(started_at),
                outcome = result.trace_label(),
                "slack.api.post_message_finished"
            );
        }
    }

    fn route_text(
        &self,
        message: &SlackMessage,
        identity: &VerifiedSlackHuman,
        agent_id: AgentId,
        text: &str,
        admission: Option<&AdmissionContext>,
    ) {
        let Some(cfg) = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .config
            .clone()
        else {
            return;
        };
        // Slack may deliver the same mentioned post through both `message` and
        // `app_mention`; native message identity, not event kind/id, is canonical.
        let dedup_key = message
            .ts
            .as_ref()
            .map(|ts| format!("message:{}:{ts}", message.channel_id))
            .or_else(|| message.event_id.clone());
        let Some(dedup_key) = dedup_key else {
            return;
        };
        let route = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            static_receive_route_for_message(&cfg, message).or_else(|| {
                resolve_receive_route(
                    &cfg,
                    &state,
                    &message.channel_id,
                    message.thread_ts.as_deref(),
                    message.channel_type.as_deref(),
                    &message.user_id,
                )
            })
        };
        let Some(route) = route else {
            return;
        };
        self.submit_ingress(
            &cfg,
            IngressSubmission {
                conversation: route,
                agent_id,
                sender: IngressSender {
                    policy_status: cfg
                        .sender_policy(&message.user_id)
                        .expect("sender admitted"),
                    user_id: message.user_id.clone(),
                    display_name: identity.display_name.clone(),
                    identity_alias: cfg.sender_aliases.get(&identity.user_id).cloned(),
                },
                operation: MessageOperation::Create {
                    payload: MessagePayload::Text {
                        text: text.to_owned(),
                        format: TextFormat::Plain,
                    },
                },
                external_identity: ExternalMessageIdentity {
                    event_id: message
                        .ts
                        .is_none()
                        .then(|| message.event_id.clone())
                        .flatten(),
                    message_id: message.ts.clone(),
                    revision_id: None,
                    dedup_key: Some(dedup_key),
                },
            },
            admission,
        );
    }

    /// Submit one normalized Slack occurrence through the canonical typed RPC.
    fn submit_ingress(
        &self,
        cfg: &RuntimeConfig,
        submission: IngressSubmission,
        admission: Option<&AdmissionContext>,
    ) {
        let IngressSubmission {
            conversation,
            agent_id,
            sender,
            operation,
            external_identity,
        } = submission;
        if !self.admission_authority_is_current(admission) {
            log_ingress_rejection("stale_epoch");
            return;
        }
        let IngressSender {
            user_id,
            display_name,
            identity_alias,
            policy_status,
        } = sender;
        let original_key = matches!(operation, MessageOperation::Create { .. })
            .then(|| {
                external_identity
                    .message_id
                    .as_ref()
                    .map(|message_id| PostedMessageKey::new(&conversation.channel_id, message_id))
            })
            .flatten();
        let reaction_message_ts = matches!(
            operation,
            MessageOperation::Create { .. } | MessageOperation::Edit { .. }
        )
        .then(|| external_identity.message_id.clone())
        .flatten();
        let request_id = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if admission.is_some_and(|context| !context.matches_state(&state))
                || self.shutdown.is_requested()
            {
                if let Some(admission) = admission {
                    admission.mark("stale_epoch");
                }
                return;
            }
            if !state.registered_agents.contains(&agent_id)
                || !is_route_authorized(&state, cfg, &conversation, &user_id)
                || !state.capability_active
            {
                if let Some(admission) = admission {
                    admission.mark("rejected_route");
                }
                return;
            }
            if state.pending_ingress.len() >= ROUTE_CORRELATION_LIMIT {
                drop(state);
                self.reply(
                    cfg,
                    &conversation.channel_id,
                    conversation.thread_ts.as_deref(),
                    "Tau has too many pending Slack prompts; try again later.",
                    admission,
                );
                if let Some(admission) = admission {
                    admission.mark("capacity");
                }
                return;
            }
            state.next_route_id = state.next_route_id.wrapping_add(1);
            let request_id = format!("slack-in-{}", state.next_route_id);
            let installation_team_id = admission
                .map(|context| context.installation_team_id.clone())
                .or_else(|| state.installation_team_id.clone());
            let Some(installation_team_id) = installation_team_id else {
                if let Some(admission) = admission {
                    admission.mark("rejected_route");
                }
                return;
            };
            state.pending_ingress.insert(
                request_id.clone(),
                PendingIngress {
                    agent_id: agent_id.clone(),
                    conversation: conversation.clone(),
                    user_id: user_id.clone(),
                    installation_team_id,
                    policy_status,
                    external_identity: external_identity.clone(),
                    original_key,
                    reaction_message_ts,
                    submitted_at: Instant::now(),
                    latency_trace: admission.map(|context| context.trace),
                },
            );
            request_id
        };
        let request = HarnessInputMessage::TransportMessageIngress(Box::new(
            TransportMessageIngressRequest {
                request_id: request_id.clone(),
                target_agent_id: agent_id,
                draft: transport_draft(
                    MessageEndpoint::External {
                        stable_id: Some(user_id),
                        display_name,
                        identity_alias: identity_alias.map(|value| {
                            tau_proto::ExternalIdentityAlias {
                                value,
                                authority:
                                    tau_proto::ExternalIdentityAliasAuthority::OperatorConfigured,
                            }
                        }),
                        actor_kind: ExternalActorKind::Human,
                    },
                    &conversation,
                    operation,
                    external_identity,
                    policy_status,
                    self.output.wire_tool_name(SEND_TOOL_NAME),
                ),
            },
        ));
        let sent = {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let still_current = state.pending_ingress.contains_key(&request_id)
                && admission.is_none_or(|context| context.matches_state(&state))
                && !self.shutdown.is_requested();
            still_current && self.output.send(request)
        };
        if !sent {
            self.state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .pending_ingress
                .remove(&request_id);
            self.shutdown.request();
            if let Some(admission) = admission {
                admission.mark("rejected_route");
            }
        }
        if let Some(admission) = admission {
            admission.mark(if sent { "submitted" } else { "rejected_route" });
            let trace = admission.trace;
            tracing::trace!(
                target: LOG_TARGET,
                schema = LATENCY_SCHEMA,
                connection_generation = trace.connection_generation,
                trace_seq = trace.trace_seq,
                event_class = trace.event_class,
                request_seq = request_id.as_str(),
                frame_to_submit_us = elapsed_us(admission.trace_received_at()),
                identity_us = admission.identity_us.get(),
                queue_wait_us = admission.queue_wait_us,
                output_outcome = if sent { "enqueued" } else { "writer_closed" },
                "slack.ingress.submitted"
            );
        }
    }
}

/// Match an exact static message route, including fixed-thread root creates.
fn static_receive_route_for_message(
    cfg: &RuntimeConfig,
    message: &SlackMessage,
) -> Option<SlackConversation> {
    let fixed = message
        .thread_ts
        .as_ref()
        .or(message.ts.as_ref())
        .and_then(|root| {
            cfg.thread_receives
                .get(&(message.channel_id.clone(), root.clone()))
                .and_then(|alias| cfg.conversations.get(alias))
        });
    fixed
        .or_else(|| {
            cfg.parent_receives
                .get(&message.channel_id)
                .and_then(|alias| cfg.conversations.get(alias))
        })
        .and_then(|policy| {
            let kind_matches = if message.event_type == "app_mention" {
                policy.kind != ConversationPolicyKind::Dm
                    && message
                        .channel_type
                        .as_deref()
                        .is_none_or(|kind| channel_type_matches(policy.kind, Some(kind)))
            } else {
                channel_type_matches(policy.kind, message.channel_type.as_deref())
            };
            if !kind_matches {
                return None;
            }
            let thread_ts = match &policy.thread_ts {
                Some(root)
                    if message.thread_ts.as_ref() == Some(root)
                        || (message.thread_ts.is_none() && message.ts.as_ref() == Some(root)) =>
                {
                    Some(root.clone())
                }
                Some(_) => return None,
                None => message.thread_ts.clone(),
            };
            Some(SlackConversation {
                channel_id: message.channel_id.clone(),
                thread_ts,
                kind: policy.kind,
                alias: policy.alias.clone(),
            })
        })
}

/// Match a receive route for an already-normalized native parent/thread.
fn resolve_receive_route(
    cfg: &RuntimeConfig,
    state: &State,
    channel_id: &str,
    thread_ts: Option<&str>,
    channel_type: Option<&str>,
    user_id: &str,
) -> Option<SlackConversation> {
    let fixed = thread_ts.and_then(|root| {
        cfg.thread_receives
            .get(&(channel_id.to_owned(), root.to_owned()))
            .and_then(|alias| cfg.conversations.get(alias))
    });
    if let Some(policy) = fixed.or_else(|| {
        cfg.parent_receives
            .get(channel_id)
            .and_then(|alias| cfg.conversations.get(alias))
    }) && channel_type.is_none_or(|kind| channel_type_matches(policy.kind, Some(kind)))
    {
        return Some(SlackConversation {
            channel_id: channel_id.to_owned(),
            thread_ts: policy
                .thread_ts
                .clone()
                .or_else(|| thread_ts.map(str::to_owned)),
            kind: policy.kind,
            alias: policy.alias.clone(),
        });
    }
    state.linked_dms.get(channel_id).and_then(|link| {
        (cfg.dynamic_direct_messages.is_some()
            && link.user_id == user_id
            && channel_id.starts_with('D')
            && channel_type.is_none_or(|kind| kind == "im"))
        .then(|| SlackConversation {
            channel_id: channel_id.to_owned(),
            thread_ts: thread_ts.map(str::to_owned),
            kind: ConversationPolicyKind::Dm,
            alias: DYNAMIC_DM_LABEL.to_owned(),
        })
    })
}

/// Slack's authenticated event family must agree with the configured family.
fn channel_type_matches(kind: ConversationPolicyKind, channel_type: Option<&str>) -> bool {
    match kind {
        ConversationPolicyKind::Channel => matches!(channel_type, Some("channel" | "group")),
        ConversationPolicyKind::Mpim => channel_type == Some("mpim"),
        ConversationPolicyKind::Dm => channel_type == Some("im"),
    }
}

fn current_route_key(
    state: &State,
    cfg: &RuntimeConfig,
    message: &SlackMessage,
) -> Option<SelectionRouteKey> {
    static_receive_route_for_message(cfg, message)
        .or_else(|| {
            resolve_receive_route(
                cfg,
                state,
                &message.channel_id,
                message.thread_ts.as_deref(),
                message.channel_type.as_deref(),
                &message.user_id,
            )
        })
        .map(|route| route.route_key())
}

fn static_receive_covers_dm(cfg: &RuntimeConfig, channel_id: &str) -> bool {
    cfg.parent_receives
        .get(channel_id)
        .and_then(|alias| cfg.conversations.get(alias))
        .is_some_and(|policy| policy.kind == ConversationPolicyKind::Dm)
        || cfg
            .thread_receives
            .iter()
            .any(|((conversation, _), alias)| {
                conversation == channel_id
                    && cfg
                        .conversations
                        .get(alias)
                        .is_some_and(|policy| policy.kind == ConversationPolicyKind::Dm)
            })
}

fn static_parent_receive_covers_dm(cfg: &RuntimeConfig, channel_id: &str) -> bool {
    cfg.parent_receives
        .get(channel_id)
        .and_then(|alias| cfg.conversations.get(alias))
        .is_some_and(|policy| policy.kind == ConversationPolicyKind::Dm)
}

/// Coarse conversation-id prefilter; callers must still resolve or revalidate
/// the exact stable route, kind, thread, sender, and owner.
fn conversation_has_receive_source(state: &State, cfg: &RuntimeConfig, channel_id: &str) -> bool {
    cfg.parent_receives.contains_key(channel_id)
        || cfg
            .thread_receives
            .keys()
            .any(|(conversation, _)| conversation == channel_id)
        || state.linked_dms.contains_key(channel_id)
}

/// Return whether an already-captured source route still has its exact
/// authority.
fn is_route_authorized(
    state: &State,
    cfg: &RuntimeConfig,
    route: &SlackConversation,
    user_id: &str,
) -> bool {
    if route.alias == DYNAMIC_DM_LABEL {
        return cfg.dynamic_direct_messages.is_some()
            && state
                .linked_dms
                .get(&route.channel_id)
                .is_some_and(|link| link.user_id == user_id);
    }
    let policy = route
        .thread_ts
        .as_ref()
        .and_then(|root| {
            cfg.thread_receives
                .get(&(route.channel_id.clone(), root.clone()))
                .and_then(|alias| cfg.conversations.get(alias))
        })
        .or_else(|| {
            cfg.parent_receives
                .get(&route.channel_id)
                .and_then(|alias| cfg.conversations.get(alias))
        });
    policy.is_some_and(|policy| policy.kind == route.kind)
}

/// Build the canonical conversation metadata used identically for ingress and
/// successful-send completion.
fn message_conversation(conversation: &SlackConversation) -> MessageConversation {
    MessageConversation {
        kind: match conversation.kind {
            ConversationPolicyKind::Channel => ConversationKind::Channel,
            ConversationPolicyKind::Mpim => ConversationKind::Group,
            ConversationPolicyKind::Dm => ConversationKind::Direct,
        },
        stable_id: Some(conversation.channel_id.clone()),
        display_name: Some(conversation.alias.clone()),
        thread: conversation
            .thread_ts
            .as_ref()
            .map(|thread_ts| MessageThread {
                stable_id: thread_ts.clone(),
                root: Some(MessageRef {
                    message_id: None,
                    external_message_id: Some(thread_ts.clone()),
                }),
            }),
        reply_to: None,
    }
}

fn send_destination_conversation(destination: &ConversationPolicy) -> MessageConversation {
    MessageConversation {
        kind: match destination.kind {
            ConversationPolicyKind::Channel => ConversationKind::Channel,
            ConversationPolicyKind::Mpim => ConversationKind::Group,
            ConversationPolicyKind::Dm => ConversationKind::Direct,
        },
        stable_id: Some(destination.conversation_id.clone()),
        display_name: Some(destination.alias.clone()),
        thread: destination
            .thread_ts
            .as_ref()
            .map(|thread_ts| MessageThread {
                stable_id: thread_ts.clone(),
                root: Some(MessageRef {
                    message_id: None,
                    external_message_id: Some(thread_ts.clone()),
                }),
            }),
        reply_to: None,
    }
}

fn send_destination_endpoint(destination: &ConversationPolicy) -> MessageEndpoint {
    MessageEndpoint::External {
        stable_id: None,
        display_name: Some(destination.alias.clone()),
        identity_alias: None,
        actor_kind: ExternalActorKind::Unknown,
    }
}

fn send_destination_capability(
    destination: &ConversationPolicy,
) -> TransportSendDestinationCapability {
    TransportSendDestinationCapability {
        alias: destination.alias.clone(),
        external_endpoint: send_destination_endpoint(destination),
        conversation: send_destination_conversation(destination),
    }
}

/// Build a normalized Slack draft without any model-visible prefix text.
fn transport_draft(
    external_endpoint: MessageEndpoint,
    conversation: &SlackConversation,
    operation: MessageOperation,
    external_identity: ExternalMessageIdentity,
    policy_status: SenderPolicyStatus,
    send_tool: tau_proto::ToolName,
) -> TransportMessageDraft {
    TransportMessageDraft {
        transport_name: TRANSPORT_NAME.to_owned(),
        external_endpoint,
        conversation: Some(message_conversation(conversation)),
        operation,
        identity_assurance: SenderIdentityAssurance::VerifiedAccount,
        policy_status,
        external_identity: Some(external_identity),
        ordering: None,
        occurred_at: None,
        send_tool: Some(send_tool),
    }
}

/// Return the exact external actor endpoint bound to a canonical reply route.
fn external_endpoint_for_route(route: &ReplyRoute) -> MessageEndpoint {
    MessageEndpoint::External {
        stable_id: Some(route.user_id.clone()),
        display_name: route.display_name.clone(),
        identity_alias: route.identity_alias.clone().map(|value| {
            tau_proto::ExternalIdentityAlias {
                value,
                authority: tau_proto::ExternalIdentityAliasAuthority::OperatorConfigured,
            }
        }),
        actor_kind: ExternalActorKind::Human,
    }
}

impl Drop for Extension {
    fn drop(&mut self) {
        self.retire_send_authority();
        self.shutdown.request();
    }
}

/// Shared shutdown state that supports synchronous checks and async wakeups.
struct ShutdownSignal {
    /// Fast flag used by synchronous code that cannot await.
    requested: AtomicBool,
    /// Wakes the asynchronous Socket Mode worker and backoff sleepers.
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

    /// Request shutdown and wake all current asynchronous waiters.
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

    /// Wait until either shutdown is requested or the requested delay elapses.
    ///
    /// Returns `true` when shutdown was requested and `false` when the delay
    /// elapsed first.
    async fn wait_timeout(&self, delay: Duration) -> bool {
        if delay.is_zero() {
            return self.is_requested();
        }
        tokio::select! {
            () = self.wait() => true,
            () = tokio::time::sleep(delay) => self.is_requested(),
        }
    }
}

fn socket_worker_loop(
    send_retirement: SendRetirement,
    client: Arc<dyn SlackClient>,
    output: Output,
    cfg: RuntimeConfig,
    startup: Option<WorkerStartup>,
    shutdown: Arc<ShutdownSignal>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_error) => {
            tracing::warn!(target: LOG_TARGET, failure = "runtime_unavailable", "failed to create Slack worker runtime");
            return;
        }
    };
    let ext = Arc::new(Extension::new_socket_worker_view(
        send_retirement,
        client,
        output,
        Arc::clone(&shutdown),
    ));
    let admission = AdmissionQueue::new();
    let admission_worker = Arc::clone(&admission);
    let admission_failure = Arc::clone(&admission);
    let failure_shutdown = Arc::clone(&shutdown);
    let admission_ext = Arc::clone(&ext);
    std::thread::spawn(move || {
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            admission_worker_loop(admission_ext, admission_worker);
        }))
        .is_err()
        {
            admission_failure.close();
            failure_shutdown.request();
        }
    });
    let mut backoff = INITIAL_RECONNECT_BACKOFF;
    let mut startup = startup;
    let mut connection_generation = 0_u64;
    while !shutdown.is_requested() {
        connection_generation = connection_generation.wrapping_add(1);
        match runtime.block_on(socket_worker_once(
            &ext,
            &cfg,
            startup.take(),
            &admission,
            connection_generation,
        )) {
            Ok(WorkerOutcome::ReconnectNow) => {
                tracing::warn!(target: LOG_TARGET, lifecycle = "reconnecting", "Slack Socket Mode connection ended; reconnecting");
                backoff = INITIAL_RECONNECT_BACKOFF;
            }
            Ok(WorkerOutcome::Shutdown) => break,
            Err(message) => {
                if ext
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .installation_mismatch
                {
                    ext.report_installation_restart_once();
                    tracing::warn!(
                        target: LOG_TARGET,
                        lifecycle = "stopped",
                        failure = "installation_restart_required",
                        "Slack Socket Mode worker requires restart after installation identity failure"
                    );
                    break;
                }
                ext.report_worker_startup_failure_once(&cfg, &message);
                tracing::warn!(target: LOG_TARGET, lifecycle = "degraded", failure = "socket_worker", "Slack Socket Mode worker failed; reconnecting");
                if runtime.block_on(shutdown.wait_timeout(backoff)) {
                    break;
                }
                backoff = (backoff * 2).min(MAX_RECONNECT_BACKOFF);
            }
        }
    }
    admission.close();
    let mut state = ext.state.lock().unwrap_or_else(|e| e.into_inner());
    state.worker_online = false;
}

/// Drain accepted occurrences serially without ever blocking websocket reads.
fn admission_worker_loop(ext: Arc<Extension>, queue: Arc<AdmissionQueue<AdmissionWork>>) {
    while let Some((work, _outstanding_permit)) = queue.pop() {
        let trace = LatencyTrace {
            connection_generation: work.connection_generation,
            trace_seq: work.trace_seq,
            event_class: work.event.event_class(),
        };
        tracing::trace!(
            target: LOG_TARGET,
            schema = LATENCY_SCHEMA,
            connection_generation = trace.connection_generation,
            trace_seq = trace.trace_seq,
            event_class = trace.event_class,
            queue_wait_us = elapsed_us(work.enqueued_at),
            queue_depth_bucket = work.queue_depth_bucket.as_str(),
            "slack.ingress.admission_started"
        );
        let started_at = Instant::now();
        let context = AdmissionContext {
            trace,
            received_at: work.received_at,
            ingress_epoch: work.ingress_epoch,
            config_generation: work.config_generation,
            agent_generation: work.agent_generation,
            installation_team_id: work.installation_team_id,
            queue_wait_us: elapsed_us(work.enqueued_at),
            identity_us: Cell::new(0),
            outcome: Cell::new("rejected_policy"),
        };
        let process_result = ext.admission_authority_is_current(Some(&context)).then(|| {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match work.event {
                DecodedSlackEvent::Message(message) => {
                    ext.process_slack_message_admitted(message, Some(&context));
                }
                DecodedSlackEvent::Reaction(reaction) => {
                    ext.process_slack_reaction_admitted(reaction, Some(&context));
                }
                DecodedSlackEvent::Edit(edit) => {
                    ext.process_slack_edit_admitted(edit, Some(&context));
                }
            }))
        });
        let panicked = process_result.is_some_and(|result| result.is_err());
        let outcome = if panicked {
            "rejected_policy"
        } else if !ext.admission_authority_is_current(Some(&context)) {
            "stale_epoch"
        } else {
            context.outcome.get()
        };
        tracing::trace!(
            target: LOG_TARGET,
            schema = LATENCY_SCHEMA,
            connection_generation = trace.connection_generation,
            trace_seq = trace.trace_seq,
            event_class = trace.event_class,
            duration_us = elapsed_us(started_at),
            outcome,
            "slack.ingress.admission_finished"
        );
        if panicked {
            tracing::warn!(
                target: LOG_TARGET,
                lifecycle = "degraded",
                failure = "admission_worker_panic",
                "Slack admission occurrence panicked; continuing ordered admission"
            );
            context.mark("rejected_policy");
        }
    }
}

struct WorkerStartup {
    /// Exact authenticated bot U/W identity.
    bot_user_id: String,
    /// Exact authenticated installing T workspace.
    installation_team_id: String,
    /// Validated one-use Socket Mode websocket URL.
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
    admission: &Arc<AdmissionQueue<AdmissionWork>>,
    connection_generation: u64,
) -> Result<WorkerOutcome, String> {
    let ws_url = match startup {
        Some(startup) => {
            let mut state = ext.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(error) = state
                .install_or_match_installation(startup.bot_user_id, startup.installation_team_id)
            {
                ext.send_wake.notify_lifecycle_change();
                return Err(error);
            }
            startup.socket_url
        }
        None => {
            let installation = ext.authenticated_installation(cfg)?;
            ext.match_established_installation(&installation)?;
            let ws_url = ext
                .client
                .open_socket(cfg)
                .map_err(|error| error.to_string())?;
            validate_socket_url(&ws_url)?;
            let mut state = ext.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(error) =
                state.install_or_match_installation(installation.bot_user_id, installation.team_id)
            {
                ext.send_wake.notify_lifecycle_change();
                return Err(error);
            }
            ws_url
        }
    };
    let (mut ws, _response) = tokio_tungstenite::connect_async(&ws_url)
        .await
        .map_err(|_| "Slack websocket connection failed".to_owned())?;
    tracing::info!(target: LOG_TARGET, lifecycle = "connected", "Slack Socket Mode connected");
    let connected_at = Instant::now();
    let mut hello_at = None;
    loop {
        let frame = tokio::select! {
            biased;
            () = ext.shutdown.wait() => {
                let _ = ws.close(None).await;
                return Ok(WorkerOutcome::Shutdown);
            }
            frame = ws.next() => frame,
        };
        let Some(frame) = frame else {
            return Ok(WorkerOutcome::ReconnectNow);
        };
        let frame = frame.map_err(|_| "Slack websocket frame failed".to_owned())?;
        let received_at = Instant::now();
        let trace_seq = ext.trace_seq.fetch_add(1, Ordering::Relaxed);
        let timing = SocketFrameTiming {
            connection_generation,
            trace_seq,
            received_at,
        };
        tracing::trace!(
            target: LOG_TARGET,
            schema = LATENCY_SCHEMA,
            connection_generation,
            trace_seq,
            event_class = "unsupported",
            frame_class = socket_frame_class(&frame),
            since_hello_us = elapsed_us(hello_at.unwrap_or(connected_at)),
            "slack.ws.frame_received"
        );
        if let Some(outcome) =
            handle_socket_frame(ext, cfg, &mut ws, frame, admission, timing, &mut hello_at).await?
        {
            return Ok(outcome);
        }
    }
}

async fn handle_socket_frame(
    ext: &Extension,
    cfg: &RuntimeConfig,
    ws: &mut WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    frame: Message,
    admission: &Arc<AdmissionQueue<AdmissionWork>>,
    timing: SocketFrameTiming,
    hello_at: &mut Option<Instant>,
) -> Result<Option<WorkerOutcome>, String> {
    match frame {
        Message::Text(text) => {
            handle_socket_text_frame(ext, cfg, ws, text.as_str(), admission, timing, hello_at).await
        }
        Message::Close(_) => Ok(Some(WorkerOutcome::ReconnectNow)),
        Message::Ping(payload) => {
            ws.send(Message::Pong(payload))
                .await
                .map_err(|_| "Slack websocket pong failed".to_owned())?;
            Ok(None)
        }
        Message::Pong(_) | Message::Binary(_) | Message::Frame(_) => Ok(None),
    }
}

/// Return the bounded websocket frame class used by latency traces.
fn socket_frame_class(frame: &Message) -> &'static str {
    match frame {
        Message::Text(_) => "text",
        Message::Ping(_) => "ping",
        Message::Pong(_) => "pong",
        Message::Binary(_) | Message::Frame(_) => "binary",
        Message::Close(_) => "close",
    }
}

async fn handle_socket_text_frame(
    ext: &Extension,
    cfg: &RuntimeConfig,
    ws: &mut WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    text: &str,
    admission: &Arc<AdmissionQueue<AdmissionWork>>,
    timing: SocketFrameTiming,
    hello_at: &mut Option<Instant>,
) -> Result<Option<WorkerOutcome>, String> {
    let SocketFrameTiming {
        connection_generation,
        trace_seq,
        received_at,
    } = timing;
    if text.len() > MAX_SOCKET_FRAME_BYTES {
        tracing::trace!(
            target: LOG_TARGET,
            schema = LATENCY_SCHEMA,
            connection_generation,
            trace_seq,
            event_class = "malformed",
            envelope_class = "oversized",
            duration_us = 0_u64,
            outcome = "rejected",
            "slack.ws.envelope_decoded"
        );
        tracing::warn!(target: LOG_TARGET, rejection = "oversized_frame", "dropping Slack Socket Mode frame");
        return Ok(None);
    }
    let decode_started = Instant::now();
    let action = handle_socket_text(ext, text);
    if action.envelope_class == EnvelopeClass::Hello {
        *hello_at = Some(received_at);
    }
    let event_class = action.event.as_ref().map_or_else(
        || {
            if action.envelope_class == EnvelopeClass::Malformed {
                "malformed"
            } else {
                "unsupported"
            }
        },
        DecodedSlackEvent::event_class,
    );
    tracing::trace!(
        target: LOG_TARGET,
        schema = LATENCY_SCHEMA,
        connection_generation,
        trace_seq,
        event_class,
        envelope_class = action.envelope_class.as_str(),
        duration_us = elapsed_us(decode_started),
        outcome = if action.event.is_some() {
            "supported"
        } else if action.envelope_class == EnvelopeClass::Malformed {
            "rejected"
        } else {
            "unsupported"
        },
        "slack.ws.envelope_decoded"
    );
    let authority = if action.event.is_some() {
        let state = ext.state.lock().unwrap_or_else(|error| error.into_inner());
        if !state.session_active {
            return Err("Slack admission unavailable without an active session".to_owned());
        }
        let Some(installation_team_id) = state.installation_team_id.clone() else {
            return Err("Slack admission unavailable without installation identity".to_owned());
        };
        Some((
            state.ingress_epoch,
            state.config_generation,
            state.agent_generation,
            installation_team_id,
        ))
    } else {
        None
    };
    let reservation = if action.event.is_some() {
        if action.ack_envelope_id.is_none() {
            return Err(
                "supported Slack envelope is missing an acknowledgement identity".to_owned(),
            );
        }
        match admission.reserve() {
            Ok(reservation) => {
                tracing::trace!(
                    target: LOG_TARGET,
                    schema = LATENCY_SCHEMA,
                    connection_generation,
                    trace_seq,
                    event_class,
                    outcome = "reserved",
                    queue_depth_bucket = admission.depth_bucket().as_str(),
                    "slack.ws.admission_slot_reserved"
                );
                Some(reservation)
            }
            Err(error) => {
                let outcome = match error {
                    ReserveError::Full => "full",
                    ReserveError::Closed => "worker_closed",
                };
                tracing::trace!(
                    target: LOG_TARGET,
                    schema = LATENCY_SCHEMA,
                    connection_generation,
                    trace_seq,
                    event_class,
                    outcome,
                    queue_depth_bucket = admission.depth_bucket().as_str(),
                    "slack.ws.admission_slot_reserved"
                );
                return Err(format!("Slack admission queue unavailable: {outcome}"));
            }
        }
    } else {
        None
    };
    let ack_result = if let Some(envelope_id) = &action.ack_envelope_id {
        let supported_event = action.event.is_some();
        tracing::trace!(
            target: LOG_TARGET,
            schema = LATENCY_SCHEMA,
            connection_generation,
            trace_seq,
            event_class,
            has_supported_event = supported_event,
            elapsed_us = elapsed_us(received_at),
            "slack.ws.ack_queued"
        );
        let ack_started = Instant::now();
        let result =
            finish_socket_ack(send_socket_ack(cfg, ws, envelope_id).await, supported_event);
        tracing::trace!(
            target: LOG_TARGET,
            schema = LATENCY_SCHEMA,
            connection_generation,
            trace_seq,
            event_class,
            has_supported_event = supported_event,
            duration_us = elapsed_us(ack_started),
            outcome = if result.is_ok() { "flushed" } else { "failed" },
            "slack.ws.ack_written"
        );
        result
    } else {
        Ok(())
    };
    ack_result?;
    let outcome = action.outcome();
    if let (Some(reservation), Some(event)) = (reservation, action.event) {
        let (ingress_epoch, config_generation, agent_generation, installation_team_id) =
            authority.expect("supported events capture authority before ACK");
        let queue_depth_bucket = admission.depth_bucket();
        reservation.commit(AdmissionWork {
            event,
            received_at,
            enqueued_at: Instant::now(),
            trace_seq,
            connection_generation,
            ingress_epoch,
            config_generation,
            agent_generation,
            installation_team_id,
            queue_depth_bucket,
        });
    }
    Ok(outcome)
}

async fn send_socket_ack(
    _cfg: &RuntimeConfig,
    ws: &mut WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    envelope_id: &str,
) -> Result<(), String> {
    let ack = serde_json::json!({ "envelope_id": envelope_id }).to_string();
    ws.send(Message::Text(ack.into()))
        .await
        .map_err(|_| "Slack websocket acknowledgement failed".to_owned())
}

#[derive(Default)]
struct SocketAction {
    ack_envelope_id: Option<String>,
    /// Decoded supported event, when the envelope carries one.
    event: Option<DecodedSlackEvent>,
    reconnect: bool,
    shutdown: bool,
    /// Bounded decoded envelope class retained from the sole JSON parse.
    envelope_class: EnvelopeClass,
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
        tracing::warn!(target: LOG_TARGET, rejection = "malformed_frame", "dropping invalid Slack Socket Mode JSON");
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
        envelope_class: match frame_type {
            Some("hello") => EnvelopeClass::Hello,
            Some("disconnect") => EnvelopeClass::Disconnect,
            Some("events_api") => EnvelopeClass::EventsApi,
            Some(_) => EnvelopeClass::Unknown,
            None => EnvelopeClass::Malformed,
        },
        ..Default::default()
    };
    match frame_type {
        Some("hello") => {
            let mut state = ext.state.lock().unwrap_or_else(|e| e.into_inner());
            state.worker_online = true;
            tracing::info!(target: LOG_TARGET, lifecycle = "hello", "Slack Socket Mode hello received");
        }
        Some("disconnect") => {
            let reason = value.get("reason").and_then(|value| value.as_str());
            action.reconnect =
                matches!(reason, Some("warning" | "refresh_requested")) || reason.is_none();
            action.shutdown = !action.reconnect;
        }
        Some("events_api") => {
            let installation = {
                let state = ext.state.lock().unwrap_or_else(|error| error.into_inner());
                state
                    .installation_team_id
                    .as_deref()
                    .zip(state.bot_user_id.as_deref())
                    .map(|(team, bot)| (team.to_owned(), bot.to_owned()))
            };
            let installation_matches = installation
                .as_ref()
                .is_some_and(|(team, bot)| event_matches_installation(&value, team, bot));
            if installation_matches {
                action.event = decode_socket_event(&value);
            } else {
                tracing::warn!(
                    target: LOG_TARGET,
                    rejection = "installation_context",
                    "dropping Slack event with missing, ambiguous, or mismatched installation context"
                );
            }
        }
        _ => {}
    }
    action
}

/// Prove that one Events API wrapper belongs to the currently authenticated
/// installation. Top-level event team data is intentionally not authoritative.
fn event_matches_installation(
    value: &serde_json::Value,
    expected_team: &str,
    expected_bot: &str,
) -> bool {
    let Some(payload) = value.get("payload") else {
        return false;
    };
    if let Some(context) = payload.get("context_team_id") {
        return context.as_str() == Some(expected_team);
    }
    let Some(authorizations) = payload
        .get("authorizations")
        .and_then(serde_json::Value::as_array)
    else {
        return false;
    };
    if authorizations.len() != 1 {
        return false;
    }
    let authorization = &authorizations[0];
    if authorization
        .get("team_id")
        .and_then(serde_json::Value::as_str)
        != Some(expected_team)
    {
        return false;
    }
    match authorization.get("user_id") {
        None => true,
        Some(user) => user.as_str() == Some(expected_bot),
    }
}

fn decode_socket_event(value: &serde_json::Value) -> Option<DecodedSlackEvent> {
    let payload = value.get("payload")?;
    if payload.get("type").and_then(|value| value.as_str()) != Some("event_callback") {
        return None;
    }
    let event = payload.get("event")?;
    let event_type = event.get("type").and_then(|value| value.as_str())?;
    let event_id = payload
        .get("event_id")
        .and_then(|value| value.as_str())
        .map(str::to_owned);
    if matches!(event_type, "reaction_added" | "reaction_removed") {
        let item = event.get("item");
        if item
            .and_then(|item| item.get("type"))
            .and_then(|value| value.as_str())
            != Some("message")
        {
            return None;
        }
        return item.and_then(|item| {
            Some(DecodedSlackEvent::Reaction(SlackReaction {
                event_id,
                event_type: if event_type == "reaction_added" {
                    ReactionKind::Added
                } else {
                    ReactionKind::Removed
                },
                user_id: event.get("user")?.as_str()?.to_owned(),
                reaction: event.get("reaction")?.as_str()?.to_owned(),
                channel_id: item.get("channel")?.as_str()?.to_owned(),
                message_ts: item.get("ts")?.as_str()?.to_owned(),
                thread_ts: event
                    .get("item")
                    .and_then(|item| item.get("thread_ts"))
                    .and_then(|value| value.as_str())
                    .map(str::to_owned),
            }))
        });
    }
    if event_type == "message"
        && event.get("subtype").and_then(|value| value.as_str()) == Some("message_changed")
    {
        let message = event.get("message")?;
        let previous = event.get("previous_message")?;
        let edited = message.get("edited")?;
        let message_ts = message.get("ts")?.as_str()?;
        let editor_user_id = edited.get("user")?.as_str()?;
        let thread_ts = message.get("thread_ts").and_then(|value| value.as_str());
        if previous.get("ts")?.as_str()? != message_ts
            || message.get("user")?.as_str()? != editor_user_id
            || previous.get("user")?.as_str()? != editor_user_id
            || previous.get("thread_ts").and_then(|value| value.as_str()) != thread_ts
        {
            return None;
        }
        validate_slack_ts(message_ts).ok()?;
        let revision_ts = edited.get("ts")?.as_str()?;
        validate_slack_ts(revision_ts).ok()?;
        if let Some(thread_ts) = thread_ts {
            validate_slack_ts(thread_ts).ok()?;
        }
        return Some(DecodedSlackEvent::Edit(SlackEdit {
            event_id,
            channel_id: event.get("channel")?.as_str()?.to_owned(),
            editor_user_id: editor_user_id.to_owned(),
            text: message.get("text")?.as_str()?.to_owned(),
            message_ts: message_ts.to_owned(),
            thread_ts: thread_ts.map(str::to_owned),
            revision_ts: revision_ts.to_owned(),
        }));
    }
    if !matches!(event_type, "app_mention" | "message") {
        return None;
    }
    let text = event.get("text").and_then(|value| value.as_str())?;
    let thread_ts = match event.get("thread_ts") {
        Some(value) => {
            let ts = value.as_str()?;
            validate_slack_ts(ts).ok()?;
            Some(ts.to_owned())
        }
        None => None,
    };
    Some(DecodedSlackEvent::Message(SlackMessage {
        event_id,
        channel_id: event.get("channel")?.as_str()?.to_owned(),
        channel_type: event
            .get("channel_type")
            .and_then(|value| value.as_str())
            .map(str::to_owned),
        user_id: event.get("user")?.as_str()?.to_owned(),
        text: text.to_owned(),
        event_type: event_type.to_owned(),
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
        thread_ts,
    }))
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
    run_with_client_and_scheduler(reader, writer, client, Arc::new(SystemSendScheduler))
}

/// Run the protocol client with an injected delivery scheduler.
fn run_with_client_and_scheduler<R, W>(
    reader: R,
    writer: W,
    client: Arc<dyn SlackClient>,
    scheduler: Arc<dyn SendScheduler>,
) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write + Send + 'static,
{
    let boundary = Arc::new(SendReaderBoundary::default());
    let reader = RetiringReader {
        inner: reader,
        boundary: Arc::clone(&boundary),
    };
    let install_boundary = Arc::clone(&boundary);
    let state = tau_client::TauExtensionRunner::new(SlackExtension)
        .run_detached_writer_with_state(reader, writer, move |handle| {
            let ext = Extension::new_with_scheduler(client, handle, scheduler);
            install_boundary.install(&ext);
            SlackRuntime { ext }
        })?;
    state.ext.retire_send_authority();
    state.ext.shutdown.request();
    Ok(())
}

/// Exact send-state handles installed after initial Configure.
#[derive(Clone)]
struct SendRetirement {
    state: Arc<Mutex<State>>,
    completion_publication_gate: Arc<Mutex<()>>,
    wake: Arc<SendWake>,
}

/// Reader boundary that can synchronously revoke sends before tau-client
/// performs EOF writer cleanup.
#[derive(Default)]
struct SendReaderBoundary {
    retirement: Mutex<Option<SendRetirement>>,
}

impl SendReaderBoundary {
    fn install(&self, extension: &Extension) {
        *self
            .retirement
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(SendRetirement {
            state: Arc::clone(&extension.state),
            completion_publication_gate: Arc::clone(&extension.completion_publication_gate),
            wake: Arc::clone(&extension.send_wake),
        });
    }

    fn retire(&self) {
        let retirement = self
            .retirement
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        if let Some(retirement) = retirement {
            retire_send_state(
                &retirement.state,
                &retirement.completion_publication_gate,
                &retirement.wake,
            );
        }
    }
}

/// Read adapter that turns EOF into a synchronous outbound-authority barrier.
struct RetiringReader<R> {
    inner: R,
    boundary: Arc<SendReaderBoundary>,
}

impl<R: Read> Read for RetiringReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        if read == 0 {
            self.boundary.retire();
        }
        Ok(read)
    }
}

fn retire_send_state(
    state: &Arc<Mutex<State>>,
    completion_publication_gate: &Mutex<()>,
    wake: &SendWake,
) {
    let _publication = completion_publication_gate
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    {
        let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
        state.ingress_epoch = state.ingress_epoch.wrapping_add(1);
        state.capability_active = false;
        state.capability_generation = state.capability_generation.wrapping_add(1);
        state.pending_posts.clear();
        state.clear_send_ledger();
    }
    wake.notify_lifecycle_change();
}

struct SlackExtension;

impl TauExtension for SlackExtension {
    type State = SlackRuntime;

    fn name(&self) -> &'static str {
        "tau-ext-slack"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder
            .configure_raw(handle_configure)
            .on_output_message(handle_output_message)
            .scoped_tool(
                tau_proto::ToolName::new(REGISTER_TOOL_NAME),
                |scope| {
                    let mut tool = register_tool_spec();
                    let send = scope.wire_tool(SEND_TOOL_NAME)?;
                    let conversations = scope.wire_tool(CONVERSATIONS_TOOL_NAME)?;
                    tool.description = Some(format!(
                        "Register or unregister this agent for Slack receive routes. Policy-permitted verified humans may then send prompts; dynamic DMs remain exact-allowlisted-user-bound. Registration grants no proactive authority. {conversations} reports configured receive and proactive-send policy. When replying, use {send}."
                    ));
                    Ok(tau_proto::ToolRegister {
                        tool,
                        tool_group: Some(slack_tool_group()),
                        prompt_fragment: None,
                    })
                },
                handle_tool_invocation,
            )
            .scoped_tool(
                tau_proto::ToolName::new(CONVERSATIONS_TOOL_NAME),
                |scope| {
                    let mut tool = conversations_tool_spec();
                    let send = scope.wire_tool(SEND_TOOL_NAME)?;
                    tool.description = Some(format!(
                        "List bounded pages of operator-configured Slack conversations and route policy. Returns model-facing aliases only, never native Slack IDs. Use an alias with {send} only when policy.proactive_send is true."
                    ));
                    Ok(tau_proto::ToolRegister {
                        tool,
                        tool_group: Some(slack_tool_group()),
                        prompt_fragment: None,
                    })
                },
                handle_tool_invocation,
            )
            .scoped_tool(
                tau_proto::ToolName::new(SEND_TOOL_NAME),
                |scope| {
                    let mut tool = send_tool_spec();
                    let conversations = scope.wire_tool(CONVERSATIONS_TOOL_NAME)?;
                    let react = scope.wire_tool(REACT_TOOL_NAME)?;
                    tool.description = Some(format!(
                        "Reply through an opaque Slack reply_to, or send proactively to an operator-configured alias, optionally discoverable with {conversations}. A successful result returns a message_ref usable with separately authorized {react}. Native Slack conversation and thread IDs are never accepted."
                    ));
                    Ok(tau_proto::ToolRegister {
                        tool,
                        tool_group: Some(slack_tool_group()),
                        prompt_fragment: None,
                    })
                },
                handle_tool_invocation,
            )
            .scoped_tool(
                tau_proto::ToolName::new(REACT_TOOL_NAME),
                |scope| {
                    let mut tool = react_tool_spec();
                    let send = scope.wire_tool(SEND_TOOL_NAME)?;
                    tool.description = Some(format!(
                        "Add or remove one emoji reaction on an exact Tau-issued Slack message_ref, including refs returned by {send}. Native Slack identifiers, aliases, toggle, list, and discovery are never accepted."
                    ));
                    Ok(tau_proto::ToolRegister {
                        tool,
                        tool_group: Some(slack_tool_group()),
                        prompt_fragment: None,
                    })
                },
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
                tau_proto::EventSelector::Exact(tau_proto::EventName::SESSION_STARTED),
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
            .ready_message("slack ready");
    }
}

struct SlackRuntime {
    /// Shared Slack bridge state and background-worker coordination.
    ext: Extension,
}

fn handle_configure(cx: tau_client::RawConfigureContext<'_, SlackRuntime>) -> ClientResult<()> {
    if cx.state.ext.config_frozen() {
        return Err(ClientError::handler(immutable_config_error()));
    }
    let instance_name = cx.instance_name().cloned();
    let cfg = match cx.parse_config::<ExtConfig>() {
        Ok(cfg) => cfg,
        Err(error) => {
            cx.state.ext.clear_config_after_error();
            return Err(error);
        }
    };
    let cfg = match cfg.validate(cx.secrets()) {
        Ok(cfg) => cfg,
        Err(message) => {
            cx.state.ext.clear_config_after_error();
            return Err(ClientError::handler(message));
        }
    };
    if let Err(message) = cx.state.ext.apply_config(cfg.clone()) {
        if cx.state.ext.config_frozen() {
            return Err(ClientError::handler(message));
        }
        cx.state.ext.clear_config_after_error();
        return Err(ClientError::handler(message));
    }
    cx.state
        .ext
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .instance_name = instance_name;
    cx.state.ext.request_transport_capability();
    Ok(())
}

/// Apply commit-gated ingress/send results and capability registration results.
fn handle_output_message(
    message: &tau_proto::HarnessOutputMessage,
    runtime: &mut SlackRuntime,
    handle: &ClientHandle,
) -> ClientResult<()> {
    if let tau_proto::HarnessOutputMessage::RegisterTransportCapabilityResult(result) = message
        && runtime
            .ext
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pending_capability_request
            .as_deref()
            == Some(result.request_id.as_str())
        && !result.accepted
    {
        handle.config_error(format!(
            "Slack typed transport capability registration failed: {}",
            result.error.as_deref().unwrap_or("rejected")
        ))?;
    }
    apply_output_message(message, &runtime.ext);
    Ok(())
}

/// Apply one correlated transport RPC result to private bridge state.
fn apply_output_message(message: &tau_proto::HarnessOutputMessage, ext: &Extension) {
    match message {
        tau_proto::HarnessOutputMessage::Disconnect(_) => {
            ext.retire_send_authority();
            ext.shutdown.request();
        }
        tau_proto::HarnessOutputMessage::RegisterTransportCapabilityResult(result) => {
            let mut state = ext.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.pending_capability_request.as_deref() == Some(result.request_id.as_str()) {
                state.pending_capability_request = None;
                state.capability_active = result.accepted && !state.installation_mismatch;
                state.capability_generation = state.capability_generation.wrapping_add(1);
                ext.send_wake.notify_lifecycle_change();
            }
        }
        tau_proto::HarnessOutputMessage::TransportMessageIngressResult(result) => {
            let mut state = ext.state.lock().unwrap_or_else(|error| error.into_inner());
            let Some(pending) = state.pending_ingress.remove(&result.request_id) else {
                tracing::trace!(
                    target: LOG_TARGET,
                    schema = LATENCY_SCHEMA,
                    outcome = "orphan",
                    "slack.ingress.result_received"
                );
                return;
            };
            let instance_name = state.instance_name.clone();
            if let Some(trace) = pending.latency_trace {
                let outcome = match &result.disposition {
                    tau_proto::TransportMessageIngressDisposition::Committed {
                        message_id: _,
                        outcome: tau_proto::TransportMessageIngressOutcome::Accepted,
                        canonical: _,
                        reply_activation: _,
                    } => "accepted",
                    tau_proto::TransportMessageIngressDisposition::Committed {
                        message_id: _,
                        outcome: tau_proto::TransportMessageIngressOutcome::Duplicate,
                        canonical: _,
                        reply_activation: _,
                    } => "duplicate",
                    tau_proto::TransportMessageIngressDisposition::Rejected { reason: _ } => {
                        "rejected"
                    }
                };
                tracing::trace!(
                    target: LOG_TARGET,
                    schema = LATENCY_SCHEMA,
                    connection_generation = trace.connection_generation,
                    trace_seq = trace.trace_seq,
                    event_class = trace.event_class,
                    submit_to_result_us = elapsed_us(pending.submitted_at),
                    outcome,
                    "slack.ingress.result_received"
                );
            }
            if let tau_proto::TransportMessageIngressDisposition::Committed {
                message_id,
                outcome: _,
                canonical,
                reply_activation: tau_proto::TransportReplyActivation::Active,
            } = &result.disposition
                && state.installation_team_id.as_deref()
                    == Some(pending.installation_team_id.as_str())
                && let Some((conversation, user_id, display_name, identity_alias)) = instance_name
                    .as_ref()
                    .and_then(|instance| canonical_slack_reply_route(canonical, &pending, instance))
            {
                if let Some(original_key) = pending.original_key.clone() {
                    state.insert_incoming_message(
                        original_key,
                        IncomingMessageOwner {
                            agent_id: pending.agent_id.clone(),
                            message_id: message_id.clone(),
                            conversation: conversation.clone(),
                            user_id: user_id.clone(),
                        },
                    );
                }
                state.insert_reply_route(
                    message_id.clone(),
                    ReplyRoute {
                        agent_id: pending.agent_id.clone(),
                        conversation: conversation.clone(),
                        user_id: user_id.clone(),
                        display_name,
                        identity_alias,
                        installation_team_id: pending.installation_team_id.clone(),
                        policy_status: canonical.policy_status,
                    },
                );
                if let Some(message_ts) = pending.reaction_message_ts {
                    let _ = state.insert_reaction_target(
                        message_id.as_ref().to_owned(),
                        ReactionTarget {
                            agent_id: pending.agent_id,
                            conversation,
                            message_ts,
                            installation_team_id: pending.installation_team_id,
                            authority: ReactionAuthority::Source {
                                message_id: message_id.clone(),
                                user_id,
                            },
                        },
                    );
                }
            }
        }
        tau_proto::HarnessOutputMessage::CompleteTransportSendResult(result) => {
            let mut state = ext.state.lock().unwrap_or_else(|error| error.into_inner());
            let pending = state.pending_posts.remove(&result.request_id);
            if let Some(pending) = pending {
                let completion = state
                    .send_ledger
                    .get(&pending.invoke.call_id)
                    .and_then(|entry| {
                        (entry.prepared.authority.token == pending.send_authority.token)
                            .then_some(&entry.disposition)
                    })
                    .and_then(|disposition| match disposition {
                        SendLedgerDisposition::AwaitingCompletion { request, copies } => {
                            Some((request.tool_result.clone(), *copies))
                        }
                        _ => None,
                    });
                let (durable_result, copies) = completion.unwrap_or_else(|| {
                    (
                        successful_tool_result(&pending.invoke, ""),
                        RemoteCopyPossibility::One,
                    )
                });
                let reaction_authority_current = result.message_id.is_some()
                    && state.capability_active
                    && state.session_generation == pending.send_authority.session_generation
                    && state.ingress_epoch == pending.send_authority.ingress_epoch
                    && state.config_generation == pending.send_authority.config_generation
                    && state.send_agent_generation(&pending.agent_id)
                        == pending.send_authority.agent_generation
                    && state.capability_generation == pending.send_authority.capability_generation
                    && state.instance_name.as_ref()
                        == pending.send_authority.instance_name.as_ref()
                    && state.bot_user_id.as_deref()
                        == Some(pending.send_authority.bot_user_id.as_str())
                    && state.installation_team_id.as_deref()
                        == Some(pending.send_authority.installation_team_id.as_str());
                if result.accepted {
                    if reaction_authority_current {
                        let _ = state.insert_reaction_target(
                            pending.message_ref,
                            ReactionTarget {
                                agent_id: pending.agent_id.clone(),
                                conversation: pending.conversation.clone(),
                                message_ts: pending.posted.ts.clone(),
                                installation_team_id: pending
                                    .send_authority
                                    .installation_team_id
                                    .clone(),
                                authority: pending.authority,
                            },
                        );
                        state.posted_messages.insert(
                            PostedMessageKey::new(
                                &pending.conversation.channel_id,
                                &pending.posted.ts,
                            ),
                            PostedMessageOwner {
                                agent_id: pending.agent_id,
                                thread_ts: pending.conversation.thread_ts,
                                installation_team_id: pending
                                    .send_authority
                                    .installation_team_id
                                    .clone(),
                            },
                        );
                    }
                    if let Some(entry) = state.send_ledger.get_mut(&pending.invoke.call_id)
                        && entry.prepared.authority.token == pending.send_authority.token
                    {
                        entry.disposition = SendLedgerDisposition::Completed {
                            result: Box::new(durable_result),
                            copies,
                        };
                    }
                } else {
                    if let Some(entry) = state.send_ledger.get_mut(&pending.invoke.call_id)
                        && entry.prepared.authority.token == pending.send_authority.token
                    {
                        entry.disposition = SendLedgerDisposition::DefinitiveFailure {
                            category: SendFailureCategory::CompletionRejected,
                            copies,
                        };
                    }
                    ext.output.emit(tool_error(
                        pending.invoke,
                        copies.caveat().map_or_else(
                            || SendFailureCategory::CompletionRejected.to_string(),
                            |caveat| {
                                format!("{}; {caveat}", SendFailureCategory::CompletionRejected)
                            },
                        ),
                    ));
                }
            }
        }
        _ => {}
    }
}

/// Validates and lowers only an Active first-canonical Slack source route.
///
/// Presentation comes exclusively from the committed snapshot; pending state is
/// used only to prove that stable native authority did not change in flight.
fn canonical_slack_reply_route(
    canonical: &tau_proto::CommittedTransportIngressRoute,
    pending: &PendingIngress,
    instance_name: &tau_proto::ExtensionName,
) -> Option<(SlackConversation, String, Option<String>, Option<String>)> {
    let tau_proto::CommittedTransportIngressRoute {
        target_agent_id,
        transport,
        external_endpoint,
        conversation,
        external_identity,
        identity_assurance,
        policy_status,
    } = canonical;
    let tau_proto::MessageTransportRef { name, instance } = transport;
    if target_agent_id != &pending.agent_id
        || name != "slack"
        || instance.as_ref() != Some(instance_name)
        || *identity_assurance != SenderIdentityAssurance::VerifiedAccount
        || *policy_status != pending.policy_status
        || external_identity != &pending.external_identity
    {
        return None;
    }
    let (user_id, source_display_name, identity_alias) = match external_endpoint {
        MessageEndpoint::External {
            stable_id: Some(stable_id),
            display_name,
            identity_alias,
            actor_kind: tau_proto::ExternalActorKind::Human,
        } if stable_id == &pending.user_id => (
            stable_id.clone(),
            display_name.clone(),
            identity_alias.as_ref().map(|alias| alias.value.clone()),
        ),
        MessageEndpoint::External {
            stable_id: _,
            display_name: _,
            identity_alias: _,
            actor_kind: _,
        }
        | MessageEndpoint::Agent {
            session_id: _,
            agent_id: _,
            display_name: _,
        }
        | MessageEndpoint::User => return None,
    };
    let MessageConversation {
        kind: canonical_kind,
        stable_id,
        display_name,
        thread,
        reply_to,
    } = conversation.as_ref()?;
    let channel_id = stable_id.as_ref()?;
    let alias = display_name.as_ref()?;
    let kind = match canonical_kind {
        ConversationKind::Channel => ConversationPolicyKind::Channel,
        ConversationKind::Group => ConversationPolicyKind::Mpim,
        ConversationKind::Direct => ConversationPolicyKind::Dm,
        ConversationKind::Room | ConversationKind::Unknown => return None,
    };
    let thread_ts = match thread {
        None => None,
        Some(MessageThread {
            stable_id,
            root:
                Some(MessageRef {
                    message_id: None,
                    external_message_id: Some(root_id),
                }),
        }) if root_id == stable_id => Some(stable_id.clone()),
        Some(MessageThread {
            stable_id: _,
            root:
                Some(MessageRef {
                    message_id: None,
                    external_message_id: Some(_),
                }),
        }) => return None,
        Some(MessageThread {
            stable_id: _,
            root:
                None
                | Some(MessageRef {
                    message_id: Some(_),
                    external_message_id: _,
                })
                | Some(MessageRef {
                    message_id: None,
                    external_message_id: None,
                }),
        }) => return None,
    };
    if channel_id != &pending.conversation.channel_id
        || thread_ts != pending.conversation.thread_ts
        || kind != pending.conversation.kind
        || reply_to.is_some()
    {
        return None;
    }
    Some((
        SlackConversation {
            channel_id: channel_id.clone(),
            thread_ts,
            kind,
            alias: alias.clone(),
        },
        user_id,
        source_display_name,
        identity_alias,
    ))
}

fn handle_tool_invocation(cx: tau_client::ToolContext<'_, SlackRuntime>) -> ClientResult<()> {
    let local = cx.local_tool_name().clone();
    cx.state
        .ext
        .dispatch_scoped_tool(&local, cx.invoke().clone());
    Ok(())
}

fn handle_live_event(cx: tau_client::RawEventContext<'_, SlackRuntime>) -> ClientResult<()> {
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
        Event::SessionStarted(_) => {
            {
                let mut state = cx
                    .state
                    .ext
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                state.ingress_epoch = state.ingress_epoch.wrapping_add(1);
                state.session_generation = state.session_generation.wrapping_add(1);
                state.session_active = true;
            }
            cx.state.ext.send_wake.notify_lifecycle_change();
            cx.state.ext.request_transport_capability();
        }
        Event::SessionAgentUnloaded(unloaded) => {
            cx.state.ext.unload_agent(&unloaded.agent_id);
        }
        Event::SessionShutdown(_) => {
            let _publication = cx
                .state
                .ext
                .completion_publication_gate
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let mut state = cx.state.ext.state.lock().unwrap_or_else(|e| e.into_inner());
            state.ingress_epoch = state.ingress_epoch.wrapping_add(1);
            state.agent_generation = state.agent_generation.wrapping_add(1);
            state.session_generation = state.session_generation.wrapping_add(1);
            state.session_active = false;
            state.registered_agents.clear();
            state.send_agent_generations.clear();
            state.agent_labels.clear();
            state.selected_agent_by_route.clear();
            state.pending_ingress.clear();
            state.clear_reply_routes();
            state.clear_incoming_messages();
            state.pending_posts.clear();
            state.clear_send_ledger();
            state.capability_active = false;
            state.capability_generation = state.capability_generation.wrapping_add(1);
            state.pending_capability_request = None;
            state.posted_messages.clear();
            state.clear_reaction_state();
            cx.state.ext.send_wake.notify_lifecycle_change();
        }
        _ => {}
    }
    Ok(())
}

fn immutable_config_error() -> String {
    "slack configuration is frozen after successful Socket Mode preflight or an authorized Slack post/reaction attempt; restart Tau to apply new Slack settings"
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
            "Register or unregister this agent for Slack receive routes. Policy-permitted verified humans may then send prompts; dynamic DMs remain exact-allowlisted-user-bound. Registration grants no proactive authority. When replying, use slack_send."
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
    let examples = vec![ToolExample {
        id: "send-reply".to_owned(),
        title: Some("Send a Slack reply".to_owned()),
        arguments: CborValue::Map(vec![
            example_field("message", example_text("Thanks, I’ll look into it.")),
            example_field("reply_to", example_text("msg_01JEXAMPLE")),
        ]),
        note: Some(
            "reply_to is an opaque selector, not a channel or bearer capability.".to_owned(),
        ),
        subcommand: None,
    }];
    ToolSpec {
        name: tau_proto::ToolName::new(SEND_TOOL_NAME),
        model_visible_name: Some(tau_proto::ToolName::new(SEND_TOOL_NAME)),
        description: Some(
            "Send to exactly one authenticated Slack reply route or operator-configured destination alias. Native Slack conversation and thread identifiers are never accepted from the model."
                .to_owned(),
        ),
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "message": { "type": "string" },
                "reply_to": {
                    "type": "string",
                    "description": "Opaque canonical message id from the Tau message envelope; mutually exclusive with destination"
                },
                "destination": {
                    "type": "string",
                    "pattern": CONVERSATION_ALIAS_PATTERN,
                    "maxLength": MAX_CONVERSATION_ALIAS_BYTES,
                    "description": "Configured Slack destination alias; mutually exclusive with reply_to"
                },
                "mention_source_user": {
                    "type": "boolean",
                    "default": false,
                    "description": "When true, prepend an internal mention of the verified reply source; valid only with reply_to"
                }
            },
            "required": ["message"],
            "additionalProperties": false
        })),
        format: None,
        tags: vec![tau_proto::ToolTag::new(SEND_TOOL_TAG)],
        enabled_by_default: false,
        background_support: None,
        examples,
    }
}

/// Fixed schema for explicit source-bound Slack reaction mutations.
fn react_tool_spec() -> ToolSpec {
    ToolSpec {
        name: tau_proto::ToolName::new(REACT_TOOL_NAME),
        model_visible_name: Some(tau_proto::ToolName::new(REACT_TOOL_NAME)),
        description: Some(
            "Add or remove one emoji reaction on an exact Slack message reference issued by Tau. Native Slack identifiers, aliases, toggle, list, and discovery are not accepted."
                .to_owned(),
        ),
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "message_ref": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 128,
                    "description": "Opaque message reference from a Tau Slack message envelope or successful slack_send result; never a Slack ID"
                },
                "emoji": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 77,
                    "pattern": "^[a-z0-9_+-]{1,64}(::skin-tone-[2-6])?$",
                    "description": "Slack emoji name without surrounding colons"
                },
                "action": { "type": "string", "enum": ["add", "remove"] }
            },
            "required": ["message_ref", "emoji", "action"],
            "additionalProperties": false
        })),
        format: None,
        tags: vec![tau_proto::ToolTag::new(REACT_TOOL_TAG)],
        enabled_by_default: false,
        background_support: None,
        examples: vec![ToolExample {
            id: "react-eyes".to_owned(),
            title: Some("Add an eyes reaction".to_owned()),
            arguments: CborValue::Map(vec![
                example_field("message_ref", example_text("slack-msg-v1-example")),
                example_field("emoji", example_text("eyes")),
                example_field("action", example_text("add")),
            ]),
            note: Some("Use action=remove only for a reaction this agent added.".to_owned()),
            subcommand: None,
        }],
    }
}

/// Fixed schema for bounded, on-demand configured-conversation discovery.
fn conversations_tool_spec() -> ToolSpec {
    ToolSpec {
        name: tau_proto::ToolName::new(CONVERSATIONS_TOOL_NAME),
        model_visible_name: Some(tau_proto::ToolName::new(CONVERSATIONS_TOOL_NAME)),
        description: Some(
            "List bounded pages of operator-configured Slack conversations and route policy. Returns model-facing aliases only, never native Slack IDs."
                .to_owned(),
        ),
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_DISCOVERY_PAGE_LIMIT
                },
                "cursor": {
                    "type": "string",
                    "maxLength": MAX_DISCOVERY_CURSOR_BYTES
                }
            },
            "additionalProperties": false
        })),
        format: None,
        tags: vec![tau_proto::ToolTag::new(CONVERSATIONS_TOOL_TAG)],
        enabled_by_default: false,
        background_support: None,
        examples: vec![ToolExample {
            id: "list-conversations".to_owned(),
            title: Some("List configured Slack conversations".to_owned()),
            arguments: CborValue::Map(vec![example_field(
                "limit",
                CborValue::Integer(DEFAULT_DISCOVERY_PAGE_LIMIT.into()),
            )]),
            note: Some("Pass next_cursor as cursor to request the next page.".to_owned()),
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

fn cbor_optional_bool_field(arguments: &CborValue, field: &str) -> Result<Option<bool>, String> {
    let CborValue::Map(entries) = arguments else {
        return Err("tool arguments must be an object".to_owned());
    };
    for (key, value) in entries {
        if matches!(key, CborValue::Text(key) if key == field) {
            return match value {
                CborValue::Bool(value) => Ok(Some(*value)),
                CborValue::Null => Ok(None),
                _ => Err(format!("`{field}` must be a boolean")),
            };
        }
    }
    Ok(None)
}

/// Read an optional non-negative integer argument representable as `usize`.
fn cbor_optional_usize_field(arguments: &CborValue, field: &str) -> Result<Option<usize>, String> {
    let CborValue::Map(entries) = arguments else {
        return Err("arguments must be an object".to_owned());
    };
    for (key, value) in entries {
        if let CborValue::Text(name) = key
            && name == field
        {
            return match value {
                CborValue::Integer(value) => usize::try_from(*value)
                    .map(Some)
                    .map_err(|_| format!("`{field}` must be a non-negative integer")),
                _ => Err(format!("`{field}` must be an integer")),
            };
        }
    }
    Ok(None)
}

/// Encode an alias as a bounded opaque pagination cursor without native data.
fn encode_discovery_cursor(alias: &str) -> String {
    format!(
        "v1:{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(alias)
    )
}

/// Decode and strictly validate one extension-issued pagination cursor.
fn decode_discovery_cursor(cursor: &str) -> Result<String, String> {
    let encoded = cursor
        .strip_prefix("v1:")
        .filter(|encoded| !encoded.is_empty())
        .ok_or_else(|| "`cursor` is malformed or stale".to_owned())?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| "`cursor` is malformed or stale".to_owned())?;
    let alias =
        String::from_utf8(bytes).map_err(|_| "`cursor` is malformed or stale".to_owned())?;
    valid_conversation_alias(&alias)
        .then_some(alias)
        .ok_or_else(|| "`cursor` is malformed or stale".to_owned())
}

/// Build the public, model-facing representation of one static route.
fn conversation_policy_value(policy: &ConversationPolicy, proactive_send: bool) -> CborValue {
    let kind = match policy.kind {
        ConversationPolicyKind::Channel => "channel",
        ConversationPolicyKind::Mpim => "mpim",
        ConversationPolicyKind::Dm => "dm",
    };
    let receive = match policy.receive {
        Some(ReceiveMode::MentionsOnly) => CborValue::Text("mentions_only".to_owned()),
        Some(ReceiveMode::AllMessages) => CborValue::Text("all_messages".to_owned()),
        None => CborValue::Null,
    };
    let mut fields = vec![
        example_field("alias", example_text(&policy.alias)),
        example_field("kind", example_text(kind)),
        example_field(
            "scope",
            example_text(if policy.thread_ts.is_some() {
                "fixed_thread"
            } else {
                "conversation"
            }),
        ),
    ];
    if let Some(description) = &policy.description {
        fields.push(example_field("description", example_text(description)));
    }
    fields.push((
        example_text("policy"),
        CborValue::Map(vec![
            (example_text("receive"), receive),
            (
                example_text("proactive_send"),
                CborValue::Bool(proactive_send),
            ),
        ]),
    ));
    CborValue::Map(fields)
}

fn cbor_optional_string_field(
    arguments: &CborValue,
    field: &str,
) -> Result<Option<String>, String> {
    let CborValue::Map(entries) = arguments else {
        return Err("arguments must be an object".to_owned());
    };
    for (key, value) in entries {
        if matches!(key, CborValue::Text(name) if name == field) {
            return match value {
                CborValue::Text(value) => Ok(Some(value.clone())),
                _ => Err(format!("`{field}` must be a string")),
            };
        }
    }
    Ok(None)
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

/// Mint a collision-resistant opaque reference without encoding routing data.
fn mint_message_ref() -> String {
    let mut bytes = [0_u8; 24];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    format!(
        "slack-msg-v1-{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    )
}

/// Validate the strict outbound emoji grammar without normalization.
fn valid_outbound_emoji(value: &str) -> bool {
    let (base, tone) = match value.split_once("::") {
        Some((base, tone)) => (base, Some(tone)),
        None => (value, None),
    };
    (1..=64).contains(&base.len())
        && base.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_+-".contains(&byte)
        })
        && tone.is_none_or(|tone| {
            matches!(
                tone.as_bytes(),
                [
                    b's',
                    b'k',
                    b'i',
                    b'n',
                    b'-',
                    b't',
                    b'o',
                    b'n',
                    b'e',
                    b'-',
                    b'2'..=b'6'
                ]
            )
        })
}

/// Revalidate the exact current route authority for one cached target.
fn reaction_target_authorized(state: &State, cfg: &RuntimeConfig, target: &ReactionTarget) -> bool {
    if state.installation_team_id.as_deref() != Some(target.installation_team_id.as_str()) {
        return false;
    }
    match &target.authority {
        ReactionAuthority::Source {
            message_id,
            user_id,
        } => {
            state.registered_agents.contains(&target.agent_id)
                && state.reply_routes.get(message_id).is_some_and(|route| {
                    route.agent_id == target.agent_id
                        && route.user_id == *user_id
                        && route.conversation == target.conversation
                        && state.installation_team_id.as_deref()
                            == Some(route.installation_team_id.as_str())
                })
                && is_route_authorized(state, cfg, &target.conversation, user_id)
        }
        ReactionAuthority::ConfiguredDestination { alias } => cfg
            .proactive_aliases
            .contains(alias)
            .then(|| cfg.conversations.get(alias))
            .flatten()
            .is_some_and(|policy| {
                policy.conversation_id == target.conversation.channel_id
                    && policy.thread_ts == target.conversation.thread_ts
                    && policy.kind == target.conversation.kind
            }),
    }
}

/// Convert typed reaction failures to bounded non-sensitive terminal text.
fn reaction_error_message(
    error: Option<ReactionApiError>,
    action: ReactionActionKind,
    current: bool,
    owned_before: bool,
) -> String {
    if !current {
        return "Slack message reference is unknown, stale, or unauthorized".to_owned();
    }
    match error {
        Some(ReactionApiError::AlreadyReacted) if action == ReactionActionKind::Add => {
            if owned_before {
                "Slack reaction replay could not be confirmed".to_owned()
            } else {
                "Slack reaction already exists but is not owned by this agent".to_owned()
            }
        }
        Some(ReactionApiError::AlreadyReacted) => {
            "Slack reaction failed: already_reacted".to_owned()
        }
        Some(ReactionApiError::NoReaction) => {
            "Slack reaction does not exist or is not locally owned".to_owned()
        }
        Some(ReactionApiError::RateLimited(seconds)) => {
            format!("Slack reactions are rate limited; retry after {seconds}s")
        }
        Some(ReactionApiError::MissingScope) => {
            "Slack reactions require the reactions:write scope; add it and reinstall the Slack app"
                .to_owned()
        }
        Some(ReactionApiError::Definitive(category)) => {
            format!("Slack reaction failed: {category}")
        }
        Some(ReactionApiError::OutcomeUnknown) | None => {
            "Slack reaction outcome is unknown; the request was not retried".to_owned()
        }
    }
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
    Event::ToolResult(successful_tool_result(&invoke, text))
}

/// Construct a terminal success with a structured model-facing result.
fn structured_tool_result(invoke: ToolStarted, result: CborValue) -> Event {
    let mut tool_result = successful_tool_result(&invoke, "");
    tool_result.result = result;
    Event::ToolResult(tool_result)
}

/// Construct the terminal success carried inside the durable send-completion
/// RPC.
fn successful_tool_result(invoke: &ToolStarted, text: &str) -> ToolResult {
    ToolResult {
        call_id: invoke.call_id.clone(),
        tool_name: invoke.tool_name.clone(),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text(text.to_owned()),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: Some(ToolUseState {
            status: ToolUseStatus::Success,
            status_text: "ok".to_owned(),
            ..Default::default()
        }),
        originator: invoke.originator.clone(),
    }
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
        // Slack arguments can contain externally influenced message text and
        // native-control sentinels; errors expose only the closed message.
        details: None,
        originator: invoke.originator,
    })
}

/// Return a non-terminal progress event when a duplicate shares active I/O.
fn reaction_coalesced(invoke: ToolStarted) -> Event {
    Event::ToolProgress(ToolProgress {
        call_id: invoke.call_id,
        tool_name: invoke.tool_name,
        message: Some("identical slack_react call is already in progress".to_owned()),
        progress: None,
        display: Some(ToolUseState {
            status: ToolUseStatus::InProgress,
            status_text: tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
            ..Default::default()
        }),
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
    if value.is_empty() || value.trim() != value {
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

fn validate_user_id(field: &str, value: &str) -> Result<String, String> {
    let value = validate_slack_id(field, value)?;
    if !matches!(value.as_bytes().first(), Some(b'U' | b'W')) || value.len() < 2 {
        return Err(format!(
            "slack `{field}` must contain exact U… or W… user ids"
        ));
    }
    Ok(value)
}

fn validate_team_id(field: &str, value: &str) -> Result<String, String> {
    let value = validate_slack_id(field, value)?;
    if !value.starts_with('T') || value.len() < 2 {
        return Err(format!("slack `{field}` must contain an exact T… team id"));
    }
    Ok(value)
}

fn validate_conversation_id(field: &str, value: &str) -> Result<String, String> {
    let value = validate_slack_id(field, value)?;
    if !matches!(value.as_bytes().first(), Some(b'C' | b'G' | b'D')) || value.len() < 2 {
        return Err(format!(
            "slack `{field}` must contain exact C…, G…, or D… conversation ids, never U/W user ids"
        ));
    }
    Ok(value)
}

fn validate_reaction_name(value: &str) -> Result<(), ()> {
    let (base, tone) = match value.split_once("::") {
        Some((base, tone)) if !tone.contains("::") => (base, Some(tone)),
        Some(_) => return Err(()),
        None => (value, None),
    };
    if base.is_empty()
        || base.len() > 64
        || !base
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '+' | '-'))
    {
        return Err(());
    }
    if tone.is_some_and(|tone| {
        !matches!(
            tone,
            "skin-tone-2" | "skin-tone-3" | "skin-tone-4" | "skin-tone-5" | "skin-tone-6"
        )
    }) {
        return Err(());
    }
    Ok(())
}

fn validate_slack_ts(value: &str) -> Result<(), ()> {
    let mut parts = value.split('.');
    let seconds = parts.next().unwrap_or_default();
    let micros = parts.next().unwrap_or_default();
    if seconds.is_empty()
        || micros.is_empty()
        || parts.next().is_some()
        || value.len() > 32
        || !seconds.bytes().all(|byte| byte.is_ascii_digit())
        || !micros.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(());
    }
    Ok(())
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

struct HttpSlackClient {
    agent: ureq::Agent,
}

/// Collapse ureq failures into non-secret identity/lifecycle categories.
fn slack_api_transport_error(error: &ureq::Error) -> SlackApiError {
    match error {
        ureq::Error::Timeout(_) => SlackApiError::TransportTimeout,
        ureq::Error::HostNotFound | ureq::Error::ConnectionFailed => {
            SlackApiError::TransportConnect
        }
        ureq::Error::Tls(_) => SlackApiError::TransportTls,
        _ => SlackApiError::Transport,
    }
}

/// Collapse ureq failures into non-secret post ambiguity categories.
fn slack_post_transport_error(error: &ureq::Error) -> SendFailureCategory {
    match error {
        ureq::Error::Timeout(_) => SendFailureCategory::Timeout,
        ureq::Error::HostNotFound | ureq::Error::ConnectionFailed => SendFailureCategory::Connect,
        ureq::Error::Tls(_) => SendFailureCategory::Tls,
        _ => SendFailureCategory::Transport,
    }
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
    ) -> Result<serde_json::Value, SlackApiError> {
        let url = format!("{}/{method}", cfg.api_base);
        let mut response = self
            .agent
            .post(&url)
            .header("Authorization", &format!("Bearer {token}"))
            .content_type("application/json")
            .send(body.to_string())
            .map_err(|error| slack_api_transport_error(&error))?;
        let status = response.status();
        let text = response
            .body_mut()
            .with_config()
            .limit(MAX_SLACK_API_RESPONSE_BYTES)
            .read_to_string()
            .map_err(|_| SlackApiError::Transport)?;
        parse_slack_api_response(status.as_u16(), &text)
    }

    /// Call `users.info` with a form-encoded parameter.
    ///
    /// Slack accepts this method via GET/form encoding but treats a JSON POST
    /// body as if the required `user` argument were absent.
    fn get_user(
        &self,
        cfg: &RuntimeConfig,
        user_id: &str,
    ) -> Result<serde_json::Value, SlackApiError> {
        let url = format!("{}/users.info", cfg.api_base);
        let mut response = self
            .agent
            .post(&url)
            .header("Authorization", &format!("Bearer {}", cfg.bot_token))
            .send_form([("user", user_id)])
            .map_err(|error| slack_api_transport_error(&error))?;
        let status = response.status();
        let text = response
            .body_mut()
            .with_config()
            .limit(MAX_SLACK_API_RESPONSE_BYTES)
            .read_to_string()
            .map_err(|_| SlackApiError::Transport)?;
        parse_slack_api_response(status.as_u16(), &text)
    }

    /// Call one reaction method with typed, body-safe failure handling.
    fn post_reaction(
        &self,
        cfg: &RuntimeConfig,
        action: ReactionActionKind,
        channel_id: &str,
        message_ts: &str,
        emoji: &str,
    ) -> Result<(), ReactionApiError> {
        let method = match action {
            ReactionActionKind::Add => "reactions.add",
            ReactionActionKind::Remove => "reactions.remove",
        };
        let url = format!("{}/{method}", cfg.api_base);
        let mut response = self
            .agent
            .post(&url)
            .header("Authorization", &format!("Bearer {}", cfg.bot_token))
            .content_type("application/json")
            .send(
                serde_json::json!({
                    "channel": channel_id,
                    "timestamp": message_ts,
                    "name": emoji
                })
                .to_string(),
            )
            .map_err(|_| ReactionApiError::OutcomeUnknown)?;
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .map(|seconds| seconds.clamp(1, 3_600));
        if status == 429 {
            return Err(ReactionApiError::RateLimited(retry_after.unwrap_or(1)));
        }
        if status >= 500 {
            return Err(ReactionApiError::OutcomeUnknown);
        }
        let text = response
            .body_mut()
            .with_config()
            .limit(MAX_SLACK_API_RESPONSE_BYTES)
            .read_to_string()
            .map_err(|_| ReactionApiError::OutcomeUnknown)?;
        let value: serde_json::Value =
            serde_json::from_str(&text).map_err(|_| ReactionApiError::OutcomeUnknown)?;
        if (200..300).contains(&status)
            && value.get("ok").and_then(serde_json::Value::as_bool) == Some(true)
        {
            return Ok(());
        }
        let code = value
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown_error");
        Err(match code {
            "already_reacted" => ReactionApiError::AlreadyReacted,
            "no_reaction" => ReactionApiError::NoReaction,
            "ratelimited" => ReactionApiError::RateLimited(retry_after.unwrap_or(1)),
            "missing_scope" => ReactionApiError::MissingScope,
            "fatal_error" | "internal_error" | "request_timeout" | "service_unavailable" => {
                ReactionApiError::OutcomeUnknown
            }
            "invalid_name" => ReactionApiError::Definitive("invalid emoji name"),
            "too_many_emoji" | "too_many_reactions" => {
                ReactionApiError::Definitive("reaction limit reached")
            }
            "is_archived" | "message_not_found" | "channel_not_found" | "not_found"
            | "thread_locked" | "not_reactable" => {
                ReactionApiError::Definitive("target unavailable")
            }
            "not_in_channel" | "restricted_action" | "missing_permission" => {
                ReactionApiError::Definitive("permission denied")
            }
            "invalid_auth" | "not_authed" | "account_inactive" | "token_revoked" => {
                ReactionApiError::Definitive("authentication failed")
            }
            _ => ReactionApiError::Definitive("request rejected"),
        })
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
    status_code: u16,
    text: &str,
) -> Result<serde_json::Value, SlackApiError> {
    if status_code == 429 {
        return Err(SlackApiError::RateLimited);
    }
    if status_code >= 500 {
        return Err(SlackApiError::RemoteFailure);
    }
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|_| SlackApiError::MalformedResponse)?;
    if !(200..300).contains(&status_code) {
        let classified = classify_api_error(value.get("error").and_then(serde_json::Value::as_str));
        return Err(match (status_code, classified) {
            (_, SlackApiError::RemoteFailure) => match status_code {
                401 => SlackApiError::Authentication,
                403 => SlackApiError::PermissionDenied,
                404 => SlackApiError::TargetUnavailable,
                _ => SlackApiError::InvalidRequest,
            },
            (_, classified) => classified,
        });
    }
    if value.get("ok").and_then(|value| value.as_bool()) != Some(true) {
        return Err(classify_api_error(
            value.get("error").and_then(serde_json::Value::as_str),
        ));
    }
    Ok(value)
}

impl SlackClient for HttpSlackClient {
    fn open_socket(&self, cfg: &RuntimeConfig) -> Result<String, SlackApiError> {
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
            .ok_or(SlackApiError::MalformedResponse)
    }

    fn auth_test(&self, cfg: &RuntimeConfig) -> Result<SlackInstallationIdentity, SlackApiError> {
        let value = self.post(cfg, "auth.test", &cfg.bot_token, serde_json::json!({}))?;
        installation_from_response(&value)
    }

    fn verified_human_identity(
        &self,
        cfg: &RuntimeConfig,
        user_id: &str,
    ) -> Result<Option<VerifiedSlackHuman>, SlackApiError> {
        let value = self.get_user(cfg, user_id)?;
        verified_human_from_response(&value, user_id)
    }

    fn post_message(
        &self,
        cfg: &RuntimeConfig,
        body: &FrozenPostBody,
    ) -> PostAttemptOutcome<PostedMessage> {
        let url = format!("{}/chat.postMessage", cfg.api_base);
        let response = self
            .agent
            .post(&url)
            .header("Authorization", &format!("Bearer {}", cfg.bot_token))
            .content_type("application/json")
            .send(body.wire_json());
        let mut response = match response {
            Ok(response) => response,
            Err(error) => {
                return PostAttemptOutcome::OutcomeUnknown(slack_post_transport_error(&error));
            }
        };
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok());
        if status == 429 {
            return PostAttemptOutcome::RateLimited(parse_retry_after(retry_after));
        }
        if status == 408 {
            return PostAttemptOutcome::OutcomeUnknown(SendFailureCategory::Timeout);
        }
        if status >= 500 {
            return PostAttemptOutcome::OutcomeUnknown(SendFailureCategory::ServiceUnavailable);
        }
        if !(200..300).contains(&status) {
            return PostAttemptOutcome::DefinitiveFailure(SendFailureCategory::InvalidRequest);
        }
        let text = match response
            .body_mut()
            .with_config()
            .limit(MAX_SLACK_API_RESPONSE_BYTES)
            .read_to_string()
        {
            Ok(text) => text,
            Err(_) => {
                return PostAttemptOutcome::OutcomeUnknown(SendFailureCategory::Transport);
            }
        };
        let value: serde_json::Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(_) => {
                return PostAttemptOutcome::OutcomeUnknown(SendFailureCategory::MalformedResponse);
            }
        };
        if value.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
            return match classify_post_api_error(
                value.get("error").and_then(serde_json::Value::as_str),
            ) {
                PostAttemptFailure::Definitive(category) => {
                    PostAttemptOutcome::DefinitiveFailure(category)
                }
                PostAttemptFailure::OutcomeUnknown(category) => {
                    PostAttemptOutcome::OutcomeUnknown(category)
                }
                PostAttemptFailure::RateLimited(delay) => PostAttemptOutcome::RateLimited(delay),
            };
        }
        let posted = match posted_message_from_response(&value) {
            Ok(posted) => posted,
            Err(PostAttemptFailure::Definitive(category)) => {
                return PostAttemptOutcome::DefinitiveFailure(category);
            }
            Err(PostAttemptFailure::OutcomeUnknown(category)) => {
                return PostAttemptOutcome::OutcomeUnknown(category);
            }
            Err(PostAttemptFailure::RateLimited(delay)) => {
                return PostAttemptOutcome::RateLimited(delay);
            }
        };
        if posted.channel_id != body.channel_id() || posted.thread_ts.as_deref() != body.thread_ts()
        {
            return PostAttemptOutcome::DefinitiveFailure(SendFailureCategory::ConflictingRoute);
        }
        PostAttemptOutcome::Accepted(posted)
    }

    fn react(
        &self,
        cfg: &RuntimeConfig,
        action: ReactionActionKind,
        channel_id: &str,
        message_ts: &str,
        emoji: &str,
    ) -> Result<(), ReactionApiError> {
        self.post_reaction(cfg, action, channel_id, message_ts, emoji)
    }
}

fn installation_from_response(
    value: &serde_json::Value,
) -> Result<SlackInstallationIdentity, SlackApiError> {
    let bot_user_id = value
        .get("user_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or(SlackApiError::MalformedResponse)?;
    let team_id = value
        .get("team_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or(SlackApiError::MalformedResponse)?;
    Ok(SlackInstallationIdentity {
        bot_user_id,
        team_id,
    })
}

fn verified_human_from_response(
    value: &serde_json::Value,
    expected_user_id: &str,
) -> Result<Option<VerifiedSlackHuman>, SlackApiError> {
    let user = value.get("user").ok_or(SlackApiError::MalformedResponse)?;
    let human = expected_user_id != "USLACKBOT"
        && user.get("id").and_then(|value| value.as_str()) == Some(expected_user_id)
        && user.get("deleted").and_then(|value| value.as_bool()) == Some(false)
        && user.get("is_bot").and_then(|value| value.as_bool()) == Some(false)
        && user.get("is_app_user").and_then(|value| value.as_bool()) == Some(false);
    if !human {
        return Ok(None);
    }
    let display_name = user
        .get("profile")
        .and_then(|profile| profile.get("display_name"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 256
                && value.chars().count() <= 80
                && value
                    .chars()
                    .all(|character| !tau_proto::requires_visible_escape(character))
        })
        .map(str::to_owned);
    Ok(Some(VerifiedSlackHuman {
        user_id: expected_user_id.to_owned(),
        display_name,
    }))
}

#[cfg(test)]
fn human_user_from_response(
    value: &serde_json::Value,
    expected_user_id: &str,
) -> Result<bool, SlackApiError> {
    verified_human_from_response(value, expected_user_id).map(|identity| identity.is_some())
}

fn posted_message_from_response(
    value: &serde_json::Value,
) -> Result<PostedMessage, PostAttemptFailure> {
    let channel_id = value
        .get("channel")
        .and_then(|value| value.as_str())
        .filter(|channel| validate_slack_id("channel", channel).is_ok())
        .ok_or(PostAttemptFailure::OutcomeUnknown(
            SendFailureCategory::MalformedResponse,
        ))?
        .to_owned();
    let ts = value
        .get("ts")
        .and_then(|value| value.as_str())
        .filter(|ts| validate_slack_ts(ts).is_ok())
        .ok_or(PostAttemptFailure::OutcomeUnknown(
            SendFailureCategory::MalformedResponse,
        ))?
        .to_owned();
    let thread_ts = match value
        .get("message")
        .and_then(|message| message.get("thread_ts"))
    {
        None => None,
        Some(value) => {
            let Some(thread_ts) = value.as_str() else {
                return Err(PostAttemptFailure::OutcomeUnknown(
                    SendFailureCategory::MalformedResponse,
                ));
            };
            if validate_slack_ts(thread_ts).is_err() {
                return Err(PostAttemptFailure::OutcomeUnknown(
                    SendFailureCategory::MalformedResponse,
                ));
            }
            Some(thread_ts.to_owned())
        }
    };
    Ok(PostedMessage {
        channel_id,
        ts,
        thread_ts,
    })
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
