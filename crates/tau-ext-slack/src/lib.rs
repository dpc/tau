//! Personal Slack Socket Mode bridge extension for Tau agents.
//!
//! The extension declares logical `slack_register`, `slack_conversations`,
//! `slack_send`, and separately authorized `slack_react` tools,
//! which `ToolNameScope` maps to final per-instance wire names. Proactive
//! destination authorization follows `SPEC-tau-ext-slack-conversation-routing`.
//! It is disabled by default, requires Slack token secrets plus a non-empty
//! allowlist, and treats Slack text as external untrusted prompt input.
//! Reply routing follows
//! `SPEC-tau-ext-slack-conversation-routing`.
//! Outbound retry and replay follow
//! `SPEC-tau-ext-slack-send-delivery`.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::error::Error;
use std::future::Future;
use std::io::{Read, Write};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use base64::{Engine as _, engine as path_base64_engine};
use futures_util::{SinkExt, StreamExt};
use tau_client::{ClientError, ClientHandle, ClientResult, ExtensionBuilder, TauExtension};
use tau_proto::{
    AgentId, CborValue, Event, HarnessInputMessage, MessageAgentTarget, MessageConversation,
    MessageDeleted, MessageDelivered, MessageEdited, MessageExtensionData, MessageFactId,
    MessageFactRef, MessageParty, MessageReactionAdded, MessageReactionRemoved, MessageSenderAuth,
    MessageSent, NoticeLevel, ToolError, ToolExample, ToolProgress, ToolResult, ToolSpec,
    ToolStarted, ToolUseState, ToolUseStatus,
};
use tokio::{runtime as path_tokio_runtime, sync as path_tokio_sync, time as path_tokio_time};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use ureq::tls as path_ureq_tls;

mod admission;
mod admission_trace;
mod pending_ingress;
mod posted_message_cache;
mod reactions;
mod send_delivery;
mod transport_mentions;

use admission::{AdmissionQueue, OutstandingPermit, QueueDepthBucket, ReserveError};
use admission_trace::{AdmissionOutcome, EventClass, LatencyTrace};
use pending_ingress::{
    OccurrenceDisposition, PendingIngress, PendingIngressKind, PendingMessageAuthority,
    SlackReportId,
};
use posted_message_cache::{PostedMessageCache, PostedMessageKey, PostedMessageOwner};
use reactions::{
    ReactionAuthority, ReactionClient, ReactionState, ReactionTarget, UnavailableReactionClient,
    react_tool_spec,
};
use send_delivery::{
    FrozenPostBody, PostAttemptFailure, PostAttemptOutcome, SendFailureCategory, SendLedgerEntry,
    SendQueueReservation, SendScheduler, SendWake, SlackApiError, SlackPostMode,
    SystemSendScheduler, classify_api_error, classify_post_api_error, parse_retry_after,
};
use transport_mentions::{
    NormalizedTransportMention, SLACK_BRIDGE_REFERENCE, normalize_transport_mentions,
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
const MAX_EVENT_ID_BYTES: usize = 256;
const RECEIVED_OCCURRENCE_LIMIT: usize = 4096;
const POSTED_MESSAGE_CACHE_SIZE: usize = 1024;
const REPLY_ROUTE_LIMIT: usize = 1024;
const SEND_LEDGER_LIMIT: usize = 1024;
const ACTIVE_SEND_WORKER_LIMIT: usize = 64;
const CONVERSATION_LIMIT: usize = 64;
const CONVERSATION_ALIAS_PATTERN: &str = "^[a-z][a-z0-9_-]{0,63}$";
const MAX_CONVERSATION_ALIAS_BYTES: usize = 64;
const DEFAULT_DISCOVERY_PAGE_LIMIT: usize = 20;
const MAX_DISCOVERY_PAGE_LIMIT: usize = 32;
const MAX_DISCOVERY_CURSOR_BYTES: usize = 128;
const MAX_DISCOVERY_RESULT_BYTES: usize = 24 * 1024;
const DYNAMIC_DM_LIMIT: usize = 64;
const DYNAMIC_DM_LABEL: &str = "direct-message";
const MAX_DIAGNOSTIC_BYTES: usize = 512;
const MAX_SOCKET_FRAME_BYTES: usize = 256 * 1024;
const INITIAL_RECONNECT_BACKOFF: Duration = Duration::from_secs(1);
const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(30);
const SOCKET_PING_INTERVAL: Duration = Duration::from_secs(10);
const SOCKET_PONG_TIMEOUT: Duration = Duration::from_secs(40);
const SOCKET_HEARTBEAT_TIMEOUT_ERROR: &str = "Slack websocket heartbeat timed out";
const LATENCY_SCHEMA: &str = "slack_latency_v1";

/// Return Slack-owned payload bounds for individual frames and complete
/// messages.
fn socket_websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_frame_size(Some(MAX_SOCKET_FRAME_BYTES))
        .max_message_size(Some(MAX_SOCKET_FRAME_BYTES))
}

/// Socket Mode heartbeat timing used to detect silently stale connections.
#[derive(Clone, Copy)]
struct SocketHeartbeat {
    /// Nonzero interval between client-originated WebSocket Ping frames.
    ping_interval: Duration,
    /// Maximum time without any WebSocket Pong before reconnecting; this must
    /// exceed `ping_interval`.
    pong_timeout: Duration,
}

impl Default for SocketHeartbeat {
    fn default() -> Self {
        Self {
            ping_interval: SOCKET_PING_INTERVAL,
            pong_timeout: SOCKET_PONG_TIMEOUT,
        }
    }
}

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
    outcome: Cell<AdmissionOutcome>,
    /// Outstanding pre-ACK slot retained until canonical confirmation.
    permit: RefCell<Option<OutstandingPermit<AdmissionWork>>>,
}

/// Exact retained ownership captured while admitting one Slack deletion.
#[derive(Clone, Eq, PartialEq)]
enum DeleteOwner {
    /// A Slack-authored message whose receive authority must remain current.
    Incoming(IncomingMessageOwner),
    /// A bridge-authored post whose provenance survives receive unregister.
    Posted(PostedMessageOwner),
}

impl DeleteOwner {
    fn agent_id(&self) -> &AgentId {
        match self {
            Self::Incoming(owner) => &owner.agent_id,
            Self::Posted(owner) => &owner.agent_id,
        }
    }

    fn message_id(&self) -> &MessageFactId {
        match self {
            Self::Incoming(owner) => &owner.message_id,
            Self::Posted(owner) => &owner.message_id,
        }
    }

    fn conversation(&self) -> &SlackConversation {
        match self {
            Self::Incoming(owner) => &owner.conversation,
            Self::Posted(owner) => &owner.conversation,
        }
    }
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
    fn mark(&self, outcome: AdmissionOutcome) {
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
        if max_message_bytes == 0 || MAX_MESSAGE_BYTES < max_message_bytes {
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
    /// `SPEC-tau-ext-slack-ingress`.
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
}

/// Local sender-admission class retained only for Slack reply authorization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SenderPolicyStatus {
    /// The sender is explicitly allowlisted.
    Allowlisted,
    /// Lax mode admitted the verified sender.
    LaxPermitted,
}

/// One transient message report selected after Slack-local admission.
enum IngressReport {
    /// A newly delivered external message.
    Delivered {
        /// Publisher-scoped identifier derived from Slack native identity.
        message_id: MessageFactId,
        /// Original normalized Slack body.
        text: String,
    },
    /// Replacement text for one known delivered message.
    Edited {
        /// Previously submitted delivered-message identifier.
        target: MessageFactId,
        /// Original normalized replacement body.
        text: String,
    },
    /// A reaction added to one known message.
    ReactionAdded {
        /// Previously submitted base-message identifier.
        target: MessageFactId,
        /// Slack reaction name.
        reaction: String,
    },
    /// A reaction removed from one known message.
    ReactionRemoved {
        /// Previously submitted base-message identifier.
        target: MessageFactId,
        /// Slack reaction name.
        reaction: String,
    },
}

/// Fully normalized Slack occurrence ready for report submission.
struct IngressSubmission {
    /// Slack-native occurrence identity used for stable report correlation.
    occurrence_key: String,
    /// Exact currently authorized native conversation.
    conversation: SlackConversation,
    /// Live registered agent selected for the occurrence.
    agent_id: AgentId,
    /// Verified sender and policy.
    sender: IngressSender,
    /// Transient report payload chosen by Slack.
    report: IngressReport,
    /// Native message timestamp used only for Slack-local route ownership.
    native_message_ts: Option<String>,
}

/// Command prefix and remainder parsed from normalized Slack text.
struct ParsedSlackCommand<'a> {
    /// Recognized or command-shaped first token.
    name: Option<&'a str>,
    /// Text after the first token.
    rest: &'a str,
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
/// `SPEC-tau-ext-slack-conversation-routing`.
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

/// Build the inert conversation description included in submitted reports.
fn message_fact_conversation(conversation: &SlackConversation) -> MessageConversation {
    MessageConversation {
        stable_id: conversation.channel_id.clone(),
        display_name: Some(conversation.alias.clone()),
        alias: (conversation.alias != DYNAMIC_DM_LABEL).then(|| conversation.alias.clone()),
    }
}

/// Derive the opaque model/tool reference required by
/// `SPEC-external-message-reports-and-facts`.
fn slack_message_fact_id(channel_id: &str, native_id: &str) -> MessageFactId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"tau-ext-slack/message-ref/v1\0");
    hasher.update(channel_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(native_id.as_bytes());
    MessageFactId::new(format!("slack-message:{}", hasher.finalize().to_hex()))
}

/// Return the stable native occurrence key shared by duplicate suppression and
/// canonical report correlation.
fn received_message_key(message: &SlackMessage) -> Option<String> {
    message
        .ts
        .as_ref()
        .map(|ts| format!("message:{}:{ts}", message.channel_id))
        .or_else(|| message.event_id.as_ref().map(|id| format!("event:{id}")))
}

/// Derive an opaque canonical sender reference without exposing a Slack user
/// ID.
fn slack_sender_ref(installation_team_id: &str, user_id: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"tau-ext-slack/sender-ref/v1\0");
    hasher.update(installation_team_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(user_id.as_bytes());
    format!("slack-sender:{}", hasher.finalize().to_hex())
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
    /// Slack event id used for process-local repeat suppression.
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

/// Validated Slack `message_deleted` mutation.
struct SlackDelete {
    /// Slack event id used for process-local repeat suppression.
    event_id: Option<String>,
    /// Authorized conversation containing the original message.
    channel_id: String,
    /// Stable timestamp/id of the deleted logical message.
    message_ts: String,
    /// Optional original thread root.
    thread_ts: Option<String>,
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
    /// Immutable deletion occurrence referencing an earlier create.
    Delete(SlackDelete),
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
    fn event_class(&self) -> EventClass {
        match self {
            Self::Message(message) if message_is_local_command(message) => EventClass::LocalCommand,
            Self::Message(_) => EventClass::Create,
            Self::Reaction(_) => EventClass::Reaction,
            Self::Edit(_) => EventClass::Edit,
            Self::Delete(_) => EventClass::Delete,
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

/// Opaque source route installed after report submission succeeds.
#[derive(Clone)]
struct ReplyRoute {
    /// Agent allowed to use this route.
    agent_id: AgentId,
    /// Private native destination never accepted from tool arguments.
    conversation: SlackConversation,
    /// Verified Slack account bound to the original route.
    user_id: String,
    /// UI-only display snapshot from the accepted source occurrence.
    display_name: Option<String>,
    /// Operator alias snapshot from the accepted source occurrence.
    identity_alias: Option<String>,
    /// Installation team bound to this source occurrence.
    installation_team_id: String,
}

/// Locally submitted incoming Slack create report eligible for later edit
/// references.
#[derive(Clone, Eq, PartialEq)]
struct IncomingMessageOwner {
    /// Agent that received the original create.
    agent_id: AgentId,
    /// Publisher-scoped id of the original create report.
    message_id: MessageFactId,
    /// Exact source-bound conversation and thread.
    conversation: SlackConversation,
    /// Verified original sender account.
    user_id: String,
}

/// Bounded FIFO membership set of recent Slack-native occurrence ids received
/// by this extension process.
#[derive(Default)]
struct ReceivedOccurrenceCache {
    /// Exact recent occurrence ids used for constant-time repeat checks.
    seen: HashSet<String>,
    /// Oldest-first occurrence ids used to enforce the fixed entry limit.
    order: VecDeque<String>,
}

impl ReceivedOccurrenceCache {
    /// Record one Slack-native occurrence and return whether it is new.
    fn insert_new(&mut self, key: String) -> bool {
        if self.seen.contains(&key) {
            return false;
        }
        self.seen.insert(key.clone());
        self.order.push_back(key);
        while self.order.len() > RECEIVED_OCCURRENCE_LIMIT {
            if let Some(old) = self.order.pop_front() {
                self.seen.remove(&old);
            }
        }
        true
    }
}

#[derive(Default)]
struct State {
    /// Immutable harness-configured extension instance for alias scoping.
    instance_name: Option<tau_proto::ExtensionName>,
    config: Option<RuntimeConfig>,
    registered_agents: HashSet<AgentId>,
    agent_labels: HashMap<AgentId, String>,
    /// Selected agent independently owned by each static route or dynamic DM.
    selected_agent_by_route: HashMap<SelectionRouteKey, AgentId>,
    /// Submitted report ids mapped to private Slack routes.
    reply_routes: HashMap<MessageFactId, ReplyRoute>,
    /// Oldest-first bound for submitted reply routes.
    reply_route_order: VecDeque<MessageFactId>,
    /// Bounded, non-evicting process/session ledger preventing replay reposts.
    send_ledger: HashMap<tau_proto::ToolCallId, SendLedgerEntry>,
    /// Monotonic send reservation token source.
    next_send_reservation: u64,
    /// Per-agent send lifecycle generations preventing unrelated churn from
    /// cancelling other agents.
    send_agent_generations: HashMap<AgentId, u64>,
    /// Shared slots reserved by delivery/retry workers.
    active_send_workers: usize,
    /// Harness session generation captured by accepted sends.
    session_generation: u64,
    /// Live per-channel pacing barrier consulted immediately before attempts.
    channel_attempt_deadlines: HashMap<String, Instant>,
    /// Logical-call FIFO per native channel. The front call retains its turn
    /// through its sole retry and provider backoff.
    channel_send_queues: HashMap<String, VecDeque<SendQueueReservation>>,
    /// Recent locally submitted incoming create reports by native Slack
    /// identity.
    incoming_messages: HashMap<PostedMessageKey, IncomingMessageOwner>,
    /// Oldest-first bound for incoming create identities.
    incoming_message_order: VecDeque<PostedMessageKey>,
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
    /// Whether the process-lifetime Socket Mode worker thread was launched.
    worker_started: bool,
    /// Whether the current WebSocket connection observed Slack's `hello`.
    worker_online: bool,
    /// Process-lifetime latch for the sole startup-or-reconnect failure notice.
    worker_connection_failure_reported: bool,
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
    /// Bounded process-local Slack occurrence ids already admitted.
    received_occurrences: ReceivedOccurrenceCache,
    /// At most 64 post-ACK reports awaiting canonical confirmation.
    pending_ingress: HashMap<SlackReportId, PendingIngress>,
    /// Recent bridge-authored Slack posts and their owning agents.
    posted_messages: PostedMessageCache,
    /// Focused source-bound reaction ownership and replay state.
    reactions: ReactionState,
}

impl State {
    /// Irreversibly revoke installation-scoped authority for this process.
    fn latch_installation_mismatch(&mut self) {
        self.ingress_epoch = self.ingress_epoch.wrapping_add(1);
        self.installation_mismatch = true;
        self.reactions.clear();
        self.clear_reply_routes();
        self.clear_incoming_messages();
        self.posted_messages.clear();
        self.linked_dms.clear();
        self.selected_agent_by_route.clear();
        self.received_occurrences = ReceivedOccurrenceCache::default();
        self.pending_ingress.clear();
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

    /// Revoke every private route or reaction state keyed to one message fact.
    fn revoke_message_authority(&mut self, message_id: &MessageFactId) {
        self.reply_routes.remove(message_id);
        self.reply_route_order
            .retain(|candidate| candidate != message_id);
        self.reactions.revoke_message(message_id);
    }

    /// Return whether live reaction ownership pins this source reply route.
    fn reply_route_is_pinned(&self, message_id: &MessageFactId) -> bool {
        self.reactions.source_route_is_pinned(message_id)
    }

    /// Insert or refresh one private reply route while evicting the oldest
    /// route.
    fn insert_reply_route(&mut self, message_id: MessageFactId, route: ReplyRoute) {
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

    /// Remove all private reply routes owned by one agent.
    fn remove_agent_reply_routes(&mut self, agent_id: &AgentId) {
        self.reply_routes
            .retain(|_, route| &route.agent_id != agent_id);
        self.reply_route_order
            .retain(|id| self.reply_routes.contains_key(id));
    }

    /// Clear all private reply routes.
    fn clear_reply_routes(&mut self) {
        self.reply_routes.clear();
        self.reply_route_order.clear();
    }

    /// Remember one locally submitted incoming create report for later edit
    /// references.
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

    /// Remove all canonically confirmed incoming identities owned by one agent.
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

    /// Clear all canonically confirmed incoming identities.
    fn clear_incoming_messages(&mut self) {
        self.incoming_messages.clear();
        self.incoming_message_order.clear();
    }

    /// Clear the process/session send horizon after the harness retires it.
    fn clear_send_ledger(&mut self) {
        self.send_ledger.clear();
        self.channel_attempt_deadlines.clear();
        self.channel_send_queues.clear();
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

    fn request_notice(&self, message: impl Into<String>, level: NoticeLevel) {
        let _ = self.send(HarnessInputMessage::ExtensionNoticeRequest(
            tau_proto::ExtensionNoticeRequest {
                message: message.into(),
                level,
            },
        ));
    }

    /// Submit one transient tool progress observation.
    fn report_tool_progress(&self, progress: ToolProgress) {
        let _ = self.send(HarnessInputMessage::emit_with_persist(
            Event::ToolProgressReported(progress),
            false,
        ));
    }

    /// Submit one terminal tool report through the typed client helper or the
    /// equivalent explicit transient channel frame.
    fn report_tool_terminal(&self, event: Event) {
        let outcome = match tau_client::ToolTerminalOutcome::try_from(event) {
            Ok(outcome) => outcome,
            Err(event) => {
                tracing::error!(event = %event.name(), "Slack tool returned non-terminal event");
                return;
            }
        };
        match self {
            Self::Client(handle) => {
                let _ = handle.report_tool_terminal_detached(outcome);
            }
            Self::Channel(tx) => {
                let _ = tx.send(HarnessInputMessage::emit_with_persist(
                    outcome.into_reported_event(),
                    false,
                ));
            }
        }
    }

    /// Write and flush one successful terminal report before returning.
    fn report_tool_result_confirmed(&self, result: ToolResult) -> bool {
        match self {
            Self::Channel(tx) => tx
                .send(HarnessInputMessage::emit_with_persist(
                    Event::ToolResultReported(result),
                    false,
                ))
                .is_ok(),
            Self::Client(handle) => handle.report_tool_result(result).is_ok(),
        }
    }
}

struct Extension {
    state: Arc<Mutex<State>>,
    /// Shared sent/delete/reaction confirmed-submission and
    /// lifecycle/fatal-output retirement barrier.
    output_submission_gate: Arc<Mutex<()>>,
    /// Slack transport, identity, and message-posting operations.
    client: Arc<dyn SlackClient>,
    /// Separately injected outbound reaction API boundary.
    reaction_client: Arc<dyn ReactionClient>,
    output: Output,
    shutdown: Arc<ShutdownSignal>,
    /// Event-driven cancellation for delivery retry waits.
    send_wake: Arc<SendWake>,
    /// Injectable delivery scheduler used by deterministic tests.
    send_scheduler: Arc<dyn SendScheduler>,
    /// Fail-closed latch set before known protocol-output retirement.
    output_failed: Arc<AtomicBool>,
    /// Process-local ordinal for private TRACE correlation.
    trace_seq: AtomicU64,
    /// Deterministic unit-test synchronization at otherwise unobservable races.
    #[cfg(test)]
    test_hooks: Arc<ExtensionTestHooks>,
}

/// Whether dispatch still owes the local protocol writer one tool terminal.
enum ToolTerminalSubmission {
    /// Dispatch must submit this ordinary terminal or progress event.
    Pending(Box<Event>),
    /// The handler already confirmed this terminal through the protocol writer.
    Confirmed,
}

impl ToolTerminalSubmission {
    /// Wrap one event that still needs ordinary dispatch submission.
    fn pending(event: Event) -> Self {
        Self::Pending(Box::new(event))
    }
}

/// One test-only boundary that announces arrival and waits for explicit
/// release.
#[cfg(test)]
struct BlockingTestHook {
    /// Announces that production code reached the exact boundary.
    reached: mpsc::Sender<()>,
    /// Holds production code at the boundary until the test mutates state.
    release: mpsc::Receiver<()>,
}

/// Test-only hooks for writer, ingress, reaction, and lifecycle races.
#[cfg(test)]
#[derive(Default)]
struct ExtensionTestHooks {
    /// Runs with the output/lifecycle gate held after pending ingress
    /// insertion.
    ingress_submission_boundary: Mutex<Option<BlockingTestHook>>,
    /// Runs with the output/lifecycle gate held before exact pending replay.
    ingress_replay_boundary: Mutex<Option<BlockingTestHook>>,
    /// Runs after pending classification and before replay acquires the gate.
    ingress_replay_classified_boundary: Mutex<Option<BlockingTestHook>>,
    /// Announces immediately before a lifecycle path attempts the output gate.
    lifecycle_gate_attempt: Mutex<Option<mpsc::Sender<()>>>,
    /// Runs after the sent report write and before typed-result submission.
    sent_report_boundary: Mutex<Option<BlockingTestHook>>,
    /// Runs after a reaction result flush and before local ownership commits.
    reaction_result_boundary: Mutex<Option<BlockingTestHook>>,
    /// Runs after the failure latch is set but before submission-gate release.
    output_failure_boundary: Mutex<Option<BlockingTestHook>>,
    /// Runs immediately before deletion acquires the submission gate.
    delete_submission_boundary: Mutex<Option<BlockingTestHook>>,
}

/// Run and consume one installed test boundary.
#[cfg(test)]
fn run_blocking_test_hook(slot: &Mutex<Option<BlockingTestHook>>) {
    if let Some(hook) = slot
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
    {
        hook.reached.send(()).expect("announce test boundary");
        hook.release.recv().expect("release test boundary");
    }
}

/// Announce one deterministic lifecycle-gate acquisition attempt.
#[cfg(test)]
fn announce_test_gate_attempt(slot: &Mutex<Option<mpsc::Sender<()>>>) {
    if let Some(sender) = slot
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
    {
        sender.send(()).expect("announce gate attempt");
    }
}

impl Extension {
    /// Create an extension with an injected event-driven send scheduler.
    #[cfg(test)]
    fn new_with_scheduler(
        client: Arc<dyn SlackClient>,
        output: impl Into<Output>,
        send_scheduler: Arc<dyn SendScheduler>,
    ) -> Self {
        Self::new_with_clients_and_scheduler(
            client,
            Arc::new(UnavailableReactionClient),
            output,
            send_scheduler,
        )
    }

    /// Create an extension with independently injected Slack and reaction API
    /// boundaries.
    fn new_with_clients_and_scheduler(
        client: Arc<dyn SlackClient>,
        reaction_client: Arc<dyn ReactionClient>,
        output: impl Into<Output>,
        send_scheduler: Arc<dyn SendScheduler>,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            output_submission_gate: Arc::new(Mutex::new(())),
            client,
            reaction_client,
            output: output.into(),
            shutdown: Arc::new(ShutdownSignal::new()),
            send_wake: Arc::new(SendWake::default()),
            send_scheduler,
            output_failed: Arc::new(AtomicBool::new(false)),
            trace_seq: AtomicU64::new(0),
            #[cfg(test)]
            test_hooks: Arc::new(ExtensionTestHooks::default()),
        }
    }

    /// Build the Socket Mode worker view over the primary extension's shared
    /// lifecycle state, sent/delete submission and retirement gate, and
    /// cancellation wake.
    fn new_socket_worker_view(
        send_retirement: SendRetirement,
        client: Arc<dyn SlackClient>,
        output: Output,
        shutdown: Arc<ShutdownSignal>,
    ) -> Self {
        let SendRetirement {
            state,
            output_submission_gate,
            wake,
            output_failed,
        } = send_retirement;
        Self {
            state,
            output_submission_gate,
            client,
            reaction_client: Arc::new(UnavailableReactionClient),
            output,
            shutdown,
            send_wake: wake,
            send_scheduler: Arc::new(SystemSendScheduler),
            output_failed,
            trace_seq: AtomicU64::new(0),
            #[cfg(test)]
            test_hooks: Arc::new(ExtensionTestHooks::default()),
        }
    }

    /// Apply validated configuration before any successful preflight or post
    /// freezes it.
    fn apply_config(&self, cfg: RuntimeConfig) -> Result<(), String> {
        let _submission = self
            .output_submission_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
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
        state.clear_reply_routes();
        state.clear_incoming_messages();
        state.posted_messages.clear();
        state.reactions.clear();
        state.bot_user_id = None;
        state.installation_team_id = None;
        state.received_occurrences = ReceivedOccurrenceCache::default();
        state.pending_ingress.clear();
        self.send_wake.notify_lifecycle_change();
        Ok(())
    }

    /// Clear inactive configuration and runtime routing state after a config
    /// error.
    fn clear_config_after_error(&self) {
        let _submission = self
            .output_submission_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
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
        state.clear_reply_routes();
        state.clear_incoming_messages();
        state.pending_ingress.clear();
        state.clear_send_ledger();
        state.posted_messages.clear();
        state.linked_dms.clear();
        state.bot_user_id = None;
        state.installation_team_id = None;
        state.received_occurrences = ReceivedOccurrenceCache::default();
        state.reactions.clear();
        self.send_wake.notify_lifecycle_change();
    }

    /// Retire all outbound send authority at the process/session transport
    /// boundary before waking background workers.
    fn retire_send_authority(&self) {
        #[cfg(test)]
        announce_test_gate_attempt(&self.test_hooks.lifecycle_gate_attempt);
        retire_send_state(&self.state, &self.output_submission_gate, &self.send_wake);
    }

    /// Synchronously retire every route and remote-effect authority after the
    /// confirmed protocol writer becomes unavailable.
    fn retire_after_output_failure(&self) {
        self.output_failed.store(true, Ordering::Release);
        #[cfg(test)]
        announce_test_gate_attempt(&self.test_hooks.lifecycle_gate_attempt);
        retire_after_output_failure(
            &self.state,
            &self.output_submission_gate,
            &self.send_wake,
            &self.output_failed,
            &self.shutdown,
        );
    }

    /// Remove one unloaded agent's private receive/reaction authority.
    fn unload_agent(&self, agent_id: &AgentId) {
        #[cfg(test)]
        announce_test_gate_attempt(&self.test_hooks.lifecycle_gate_attempt);
        let _submission = self
            .output_submission_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
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
        state.remove_agent_pending_ingress(agent_id);
        state.posted_messages.remove_agent(agent_id);
        state.reactions.remove_agent(agent_id);
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

    /// Dispatch a Tau tool invocation owned by this extension.
    fn dispatch_scoped_tool(&self, local_tool_name: &tau_proto::ToolName, invoke: ToolStarted) {
        self.output.report_tool_progress(ToolProgress {
            call_id: invoke.call_id.clone(),
            tool_name: invoke.tool_name.clone(),
            message: Some("slack tool started".to_owned()),
            progress: None,
            display: Some(ToolUseState {
                status: ToolUseStatus::InProgress,
                status_text: tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
                ..Default::default()
            }),
        });
        let terminal = match local_tool_name.as_str() {
            REGISTER_TOOL_NAME => Some(ToolTerminalSubmission::pending(
                self.handle_register(invoke),
            )),
            CONVERSATIONS_TOOL_NAME => Some(ToolTerminalSubmission::pending(
                self.handle_conversations(invoke),
            )),
            SEND_TOOL_NAME => self
                .handle_send(invoke)
                .map(ToolTerminalSubmission::pending),
            REACT_TOOL_NAME => self.dispatch_reaction(invoke),
            _ => Some(ToolTerminalSubmission::pending(tool_error(
                invoke,
                "unknown slack tool".to_owned(),
            ))),
        };
        if let Some(ToolTerminalSubmission::Pending(event)) = terminal {
            let event = *event;
            if let Event::ToolProgressReported(progress) = event {
                self.output.report_tool_progress(progress);
            } else {
                self.output.report_tool_terminal(event);
            }
        }
    }

    /// Execute one reaction and identify whether its returned terminal still
    /// needs protocol submission.
    fn dispatch_reaction(&self, invoke: ToolStarted) -> Option<ToolTerminalSubmission> {
        if self.identical_reaction_call_in_flight(&invoke) {
            return None;
        }
        Some(self.handle_react(invoke))
    }

    /// Coalesce an identical concurrent delivery onto its original reaction
    /// call.
    fn identical_reaction_call_in_flight(&self, invoke: &ToolStarted) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .reactions
            .identical_call_in_flight(invoke)
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
            let _submission = self
                .output_submission_gate
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.registered_agents.remove(&invoke.agent_id) {
                state.agent_generation = state.agent_generation.wrapping_add(1);
                state.bump_send_agent_generation(&invoke.agent_id);
            }
            state
                .selected_agent_by_route
                .retain(|_, agent| agent != &invoke.agent_id);
            state.remove_agent_reply_routes(&invoke.agent_id);
            state.reactions.remove_agent_sources(&invoke.agent_id);
            state.remove_agent_incoming_messages(&invoke.agent_id);
            state.remove_agent_pending_ingress(&invoke.agent_id);
            self.send_wake.notify_lifecycle_change();
        }
        let mut result = vec![example_field(
            "status",
            example_text(if enabled {
                "registered"
            } else {
                "unregistered"
            }),
        )];
        if enabled {
            result.push(example_field(
                "incoming_transport_reference",
                example_text(SLACK_BRIDGE_REFERENCE),
            ));
        }
        structured_tool_result(invoke, CborValue::Map(result))
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
        let _submission = self
            .output_submission_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
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
        let _submission = self
            .output_submission_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
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
        state.worker_connection_failure_reported = false;
        let send_retirement = SendRetirement {
            state: Arc::clone(&self.state),
            output_submission_gate: Arc::clone(&self.output_submission_gate),
            wake: Arc::clone(&self.send_wake),
            output_failed: Arc::clone(&self.output_failed),
        };
        let output = self.output.clone();
        let client = Arc::clone(&self.client);
        let shutdown = Arc::clone(&self.shutdown);
        std::thread::spawn(move || {
            socket_worker_loop(send_retirement, client, output, cfg, startup, shutdown)
        });
        Ok(())
    }

    fn report_worker_connection_failure_once(&self, message: &str) {
        let should_report = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.worker_online || state.worker_connection_failure_reported {
                false
            } else {
                state.worker_connection_failure_reported = true;
                true
            }
        };
        if should_report {
            let message = format!(
                "Slack Socket Mode startup or reconnect failed; check std-slack tokens, Socket Mode settings, and network access: {}",
                bounded_text(message, 128)
            );
            self.output.request_notice(
                bounded_text(&message, MAX_DIAGNOSTIC_BYTES),
                NoticeLevel::Warning,
            );
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
            self.output.request_notice(
                "Slack installation identity changed or became invalid; restart Tau before using std-slack again",
                NoticeLevel::Warning,
            );
        }
    }

    #[cfg(test)]
    fn verified_human(&self, cfg: &RuntimeConfig, user_id: &str) -> bool {
        self.verified_human_traced(cfg, user_id, None).is_some()
    }

    /// Verify one sender while emitting only a bounded, payload-free latency
    /// trace.
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
                event_class = trace.event_class.as_str(),
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
                event_class = trace.event_class.as_str(),
                source = "api",
                duration_us = elapsed_us(started_at),
                outcome,
                "slack.identity.verification_finished"
            );
        }
        if !self.admission_authority_is_current(admission) {
            if let Some(admission) = admission {
                admission.mark(AdmissionOutcome::StaleEpoch);
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
                        admission.mark(AdmissionOutcome::RejectedIdentity);
                    }
                    log_ingress_rejection("sender_not_human");
                }
                identity
            }
            Err(error) => {
                if let Some(admission) = admission {
                    admission.mark(AdmissionOutcome::RejectedIdentity);
                }
                let should_report = {
                    let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    !std::mem::replace(&mut state.identity_failure_reported, true)
                };
                if should_report {
                    tracing::warn!(target: LOG_TARGET, rejection = "identity_api_failure", error = %error, "Slack ingress occurrence rejected; users.info verification degraded");
                    self.output.request_notice(
                        bounded_text(
                            &format!(
                                "Slack rejected one ingress occurrence because users.info verification failed (check users:read scope and app reinstall): {}",
                                error
                            ),
                            MAX_DIAGNOSTIC_BYTES,
                        ),
                        NoticeLevel::Warning,
                    );
                }
                None
            }
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
                message_id: slack_message_fact_id(&conversation.channel_id, &post.ts),
                thread_ts: conversation.thread_ts.clone(),
                conversation,
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
        let occurrence_key = reaction.event_id.as_ref().map_or_else(
            || {
                format!(
                    "reaction:{}:{}:{}:{}:{}",
                    reaction.event_type.as_str(),
                    reaction.channel_id,
                    reaction.message_ts,
                    reaction.user_id,
                    reaction.reaction
                )
            },
            |event_id| format!("reaction:{event_id}"),
        );
        if !self.admit_or_replay_occurrence(
            occurrence_key.clone(),
            admission,
            reaction.event_id.is_some(),
            true,
            false,
        ) {
            return;
        }
        let (cfg, agent_id, target_message_id, route) = {
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
            (cfg, owner.agent_id.clone(), owner.message_id.clone(), route)
        };
        if !self.admit_or_replay_occurrence(
            occurrence_key.clone(),
            admission,
            reaction.event_id.is_some(),
            true,
            true,
        ) {
            return;
        }
        let Some(identity) = self.verified_human_traced(&cfg, &reaction.user_id, admission) else {
            return;
        };
        self.submit_ingress(
            &cfg,
            IngressSubmission {
                occurrence_key,
                conversation: route,
                agent_id,
                sender: IngressSender {
                    user_id: reaction.user_id,
                    display_name: identity.display_name,
                    identity_alias: cfg.sender_aliases.get(&identity.user_id).cloned(),
                },
                report: match reaction.event_type {
                    ReactionKind::Added => IngressReport::ReactionAdded {
                        target: target_message_id.clone(),
                        reaction: reaction.reaction,
                    },
                    ReactionKind::Removed => IngressReport::ReactionRemoved {
                        target: target_message_id,
                        reaction: reaction.reaction,
                    },
                },
                native_message_ts: Some(reaction.message_ts),
            },
            admission,
        );
    }

    /// Route a validated edit only when its original locally submitted create
    /// report is known.
    ///
    /// This locally submitted report ownership lookup and its fail-closed
    /// rejection path implement `SPEC-tau-ext-slack-message-mutations`.
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
        let received_key = edit.event_id.clone().unwrap_or_else(|| {
            format!(
                "edit:{}:{}:{}",
                edit.channel_id, edit.message_ts, edit.revision_ts
            )
        });
        let occurrence_key = format!("edit:{received_key}");
        if !self.admit_or_replay_occurrence(occurrence_key.clone(), admission, true, true, false) {
            return;
        }
        let (cfg, owner, bot_user_id) = {
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
            let Some(bot_user_id) = state.bot_user_id.clone() else {
                return;
            };
            (cfg, owner, bot_user_id)
        };
        let text = edit.text.trim();
        if text.is_empty() || text.len() > cfg.max_message_bytes {
            log_ingress_rejection("malformed_text");
            return;
        }
        let NormalizedTransportMention { text, leading: _ } =
            normalize_transport_mentions(text, &bot_user_id);
        if text.is_empty() {
            log_ingress_rejection("malformed_text");
            return;
        }
        if !self.admit_or_replay_occurrence(occurrence_key.clone(), admission, true, true, true) {
            return;
        }
        let Some(identity) = self.verified_human_traced(&cfg, &edit.editor_user_id, admission)
        else {
            return;
        };
        self.submit_ingress(
            &cfg,
            IngressSubmission {
                occurrence_key,
                conversation: owner.conversation,
                agent_id: owner.agent_id,
                sender: IngressSender {
                    user_id: edit.editor_user_id,
                    display_name: identity.display_name,
                    identity_alias: cfg.sender_aliases.get(&identity.user_id).cloned(),
                },
                report: IngressReport::Edited {
                    target: owner.message_id,
                    text,
                },
                native_message_ts: Some(edit.message_ts),
            },
            admission,
        );
    }

    /// Submit a deletion report only for one locally retained delivered
    /// message.
    #[cfg(test)]
    fn process_slack_delete(&self, delete: SlackDelete) {
        self.process_slack_delete_admitted(delete, None);
    }

    /// Submit a deletion report only for one locally retained delivered
    /// message.
    fn process_slack_delete_admitted(
        &self,
        delete: SlackDelete,
        admission: Option<&AdmissionContext>,
    ) {
        if validate_conversation_id("delete.channel", &delete.channel_id).is_err()
            || validate_slack_ts(&delete.message_ts).is_err()
            || delete
                .thread_ts
                .as_deref()
                .is_some_and(|thread| validate_slack_ts(thread).is_err())
        {
            log_ingress_rejection("malformed_event");
            return;
        }
        let occurrence = delete.event_id.as_ref().map_or_else(
            || format!("{}:{}", delete.channel_id, delete.message_ts),
            Clone::clone,
        );
        let occurrence_key = format!("delete:{occurrence}");
        if !self.admit_or_replay_occurrence(occurrence_key.clone(), admission, true, false, false) {
            return;
        }
        let report_id = SlackReportId::from_occurrence(&occurrence_key);
        let key = PostedMessageKey::new(&delete.channel_id, &delete.message_ts);
        let (publisher, owner) = {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let Some(cfg) = state.config.as_ref() else {
                return;
            };
            let owner = state
                .incoming_messages
                .get(&key)
                .cloned()
                .map(DeleteOwner::Incoming)
                .or_else(|| {
                    state
                        .posted_messages
                        .get(&key)
                        .cloned()
                        .map(DeleteOwner::Posted)
                });
            let Some(owner) = owner else {
                log_ingress_rejection("delete_unknown_target");
                return;
            };
            let conversation = owner.conversation();
            let thread_matches = conversation.thread_ts == delete.thread_ts
                || (delete.thread_ts.is_none()
                    && conversation.thread_ts.as_deref() == Some(delete.message_ts.as_str()));
            if !thread_matches
                || (matches!(owner, DeleteOwner::Incoming(_))
                    && (!state.registered_agents.contains(owner.agent_id())
                        || !conversation_has_receive_source(&state, cfg, &delete.channel_id)))
                || matches!(
                    &owner,
                    DeleteOwner::Posted(posted)
                        if state.installation_team_id.as_deref()
                            != Some(posted.installation_team_id.as_str())
                )
                || admission.is_some_and(|context| !context.matches_state(&state))
                || self.output_failed.load(Ordering::Acquire)
                || self.shutdown.is_requested()
            {
                log_ingress_rejection("delete_owner_or_thread");
                return;
            }
            let Some(instance_name) = state.instance_name.as_ref() else {
                return;
            };
            (
                tau_proto::RawMessagePublisherId::new(instance_name.to_string()),
                owner,
            )
        };
        if !self.admit_or_replay_occurrence(occurrence_key.clone(), admission, true, false, true) {
            return;
        }
        let agent_id = owner.agent_id().clone();
        let message_id = owner.message_id().clone();
        let conversation = owner.conversation().clone();
        let mut fact = MessageDeleted::new(
            publisher.clone(),
            MessageAgentTarget::new(agent_id.to_string()),
            MessageFactRef {
                publisher_extension_id: publisher,
                message_id: message_id.clone(),
            },
            None,
            Some(message_fact_conversation(&conversation)),
        );
        fact.extension_data = report_id.extension_data();
        let event = Event::MessageDeletedReported(fact);
        let pending_report = event.clone();
        // Revoke before the local write so a concurrently dispatched reply or
        // reaction cannot begin after the deletion report becomes visible. Writer
        // failure keeps authority revoked because the remote deletion is already
        // known and restoring a stale route would violate fail-closed behavior.
        #[cfg(test)]
        run_blocking_test_hook(&self.test_hooks.delete_submission_boundary);
        let submission = self
            .output_submission_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let lifecycle_current = admission
            .is_none_or(|context| context.matches_state(&state) && state.session_active)
            && !self.output_failed.load(Ordering::Acquire)
            && !self.shutdown.is_requested();
        let owner_current = match &owner {
            DeleteOwner::Incoming(expected) => {
                lifecycle_current
                    && state.registered_agents.contains(&expected.agent_id)
                    && state.config.as_ref().is_some_and(|cfg| {
                        is_route_authorized(&state, cfg, &expected.conversation, &expected.user_id)
                    })
                    && state.incoming_messages.get(&key) == Some(expected)
            }
            DeleteOwner::Posted(expected) => {
                lifecycle_current
                    && state.installation_team_id.as_deref()
                        == Some(expected.installation_team_id.as_str())
                    && state.posted_messages.get(&key) == Some(expected)
            }
        };
        if !owner_current {
            log_ingress_rejection("delete_stale_owner");
            return;
        }
        match owner {
            DeleteOwner::Incoming(_) => {
                state.incoming_messages.remove(&key);
                state
                    .incoming_message_order
                    .retain(|candidate| candidate != &key);
            }
            DeleteOwner::Posted(_) => {
                state.posted_messages.remove(&key);
            }
        }
        state.revoke_message_authority(&message_id);
        let permit = admission.and_then(|context| context.permit.borrow_mut().take());
        let ingress_epoch = state.ingress_epoch;
        let config_generation = state.config_generation;
        let agent_generation = state.agent_generation;
        state.pending_ingress.insert(
            report_id.clone(),
            PendingIngress {
                kind: PendingIngressKind::Deleted,
                agent_id,
                message_id,
                ingress_epoch,
                config_generation,
                agent_generation,
                message_authority: None,
                report: pending_report,
                _permit: permit,
            },
        );
        drop(state);
        #[cfg(test)]
        run_blocking_test_hook(&self.test_hooks.ingress_submission_boundary);
        if self.output_failed.load(Ordering::Acquire) || self.shutdown.is_requested() {
            self.state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .pending_ingress
                .remove(&report_id);
            return;
        }
        let sent = self
            .output
            .send_confirmed(HarnessInputMessage::emit_with_persist(event, false));
        if !sent {
            self.state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .pending_ingress
                .remove(&report_id);
            self.output_failed.store(true, Ordering::Release);
            #[cfg(test)]
            run_blocking_test_hook(&self.test_hooks.output_failure_boundary);
        }
        drop(submission);
        if !sent {
            self.retire_after_output_failure();
            return;
        }
        if let Some(admission) = admission {
            admission.mark(AdmissionOutcome::Submitted);
        }
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
        if validate_conversation_id("event.channel", &message.channel_id).is_err()
            || message
                .ts
                .as_deref()
                .is_none_or(|ts| validate_slack_ts(ts).is_err())
        {
            log_ingress_rejection("malformed_event");
            return;
        }
        let occurrence_key =
            received_message_key(&message).expect("validated Slack message identity");
        if !self.admit_or_replay_occurrence(occurrence_key.clone(), admission, true, true, false) {
            return;
        }
        if message.bot_id.is_some() || message.subtype.is_some() {
            log_ingress_rejection(if message.bot_id.is_some() {
                "bot_message"
            } else {
                "unsupported_subtype"
            });
            return;
        }
        if validate_user_id("event.user", &message.user_id).is_err()
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
        if !self.admit_or_replay_occurrence(occurrence_key, admission, true, true, true) {
            return;
        }
        let Some(identity) = self.verified_human_traced(&cfg, &message.user_id, admission) else {
            return;
        };
        let bot_user_id = {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if admission.is_some_and(|context| !context.matches_state(&state)) {
                return;
            }
            state.bot_user_id.clone()
        };
        let Some(bot_user_id) = bot_user_id else {
            return;
        };
        let Some(text) = self.trimmed_message_text(&cfg, &message, admission) else {
            return;
        };
        let NormalizedTransportMention {
            text,
            leading: leading_mention,
        } = normalize_transport_mentions(&text, &bot_user_id);
        if text.is_empty() {
            self.reply(
                &cfg,
                &message.channel_id,
                message.thread_ts.as_deref(),
                help_text(),
                admission,
            );
            return;
        }
        let (name, rest) = if is_dm || leading_mention {
            parse_command(&text)
        } else {
            (None, "")
        };
        // Lax senders contribute untrusted prompt content; they never gain
        // bridge-control authority (including linking or target selection).
        if name.is_some_and(|command| {
            matches!(
                command,
                "start" | "/start" | "agents" | "/agents" | "select" | "/select" | "to" | "/to"
            ) || command.starts_with('/')
        }) && !cfg.allowed_user_ids.contains(&message.user_id)
        {
            log_ingress_rejection("sender_control_policy");
            return;
        }
        if self.rejects_unlinked_command(&cfg, &message, name, admission) {
            return;
        }
        if self.handle_command(
            &cfg,
            &message,
            &identity,
            ParsedSlackCommand { name, rest },
            admission,
        ) {
            return;
        }
        self.route_plain_text(&cfg, &message, &identity, &text, admission);
    }

    /// Classify one native occurrence without replacing an exact pending
    /// report.
    fn classify_received_occurrence(
        &self,
        key: String,
        admission: Option<&AdmissionContext>,
        deduplicate_confirmed: bool,
        require_agent_generation: bool,
        record_new: bool,
    ) -> Option<OccurrenceDisposition> {
        let current = if require_agent_generation {
            self.local_effect_authority_is_current(admission)
        } else {
            self.admission_authority_is_current(admission)
        };
        if !current {
            return None;
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let report_id = SlackReportId::from_occurrence(&key);
        if state.pending_ingress.contains_key(&report_id) {
            return Some(OccurrenceDisposition::Pending(report_id));
        }
        if deduplicate_confirmed && state.received_occurrences.seen.contains(&key) {
            return Some(OccurrenceDisposition::ConfirmedDuplicate);
        }
        if deduplicate_confirmed && record_new {
            assert!(state.received_occurrences.insert_new(key));
        }
        Some(OccurrenceDisposition::New)
    }

    /// Continue a new occurrence, replay a pending report, or suppress a
    /// confirmed duplicate before recomputing identity or routing.
    fn admit_or_replay_occurrence(
        &self,
        key: String,
        admission: Option<&AdmissionContext>,
        deduplicate_confirmed: bool,
        require_agent_generation: bool,
        record_new: bool,
    ) -> bool {
        match self.classify_received_occurrence(
            key,
            admission,
            deduplicate_confirmed,
            require_agent_generation,
            record_new,
        ) {
            Some(OccurrenceDisposition::New) => true,
            Some(OccurrenceDisposition::Pending(report_id)) => {
                #[cfg(test)]
                run_blocking_test_hook(&self.test_hooks.ingress_replay_classified_boundary);
                self.replay_pending_ingress(&report_id, admission);
                false
            }
            Some(OccurrenceDisposition::ConfirmedDuplicate) => {
                if let Some(admission) = admission {
                    admission.mark(AdmissionOutcome::DuplicateIngress);
                }
                false
            }
            None => false,
        }
    }

    /// Replay one exact pending report when Slack redelivers an occurrence
    /// before its canonical confirmation.
    fn replay_pending_ingress(
        &self,
        report_id: &SlackReportId,
        admission: Option<&AdmissionContext>,
    ) {
        let submission = self
            .output_submission_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let report = {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if admission.is_some_and(|context| !context.matches_state(&state))
                || self.output_failed.load(Ordering::Acquire)
                || self.shutdown.is_requested()
            {
                return;
            }
            state
                .pending_ingress
                .get(report_id)
                .map(|pending| pending.report.clone())
        };
        let Some(report) = report else {
            return;
        };
        #[cfg(test)]
        run_blocking_test_hook(&self.test_hooks.ingress_replay_boundary);
        if self.output_failed.load(Ordering::Acquire) || self.shutdown.is_requested() {
            return;
        }
        let sent = self
            .output
            .send_confirmed(HarnessInputMessage::emit_with_persist(report, false));
        if !sent {
            self.output_failed.store(true, Ordering::Release);
        }
        drop(submission);
        if !sent {
            self.retire_after_output_failure();
            return;
        }
        if let Some(admission) = admission {
            admission.mark(AdmissionOutcome::Submitted);
        }
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
            self.reply(
                cfg,
                &message.channel_id,
                message.thread_ts.as_deref(),
                "Only text messages are supported by this Tau bridge.",
                admission,
            );
            None
        } else if text.len() > cfg.max_message_bytes {
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
        parsed: ParsedSlackCommand<'_>,
        admission: Option<&AdmissionContext>,
    ) -> bool {
        match parsed.name {
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
                self.handle_select_command(cfg, message, parsed.rest, admission);
                true
            }
            Some("to" | "/to") => {
                self.handle_to_command(cfg, message, identity, parsed.rest, admission);
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
            Err(reply) => self.reply(
                cfg,
                &message.channel_id,
                message.thread_ts.as_deref(),
                &reply,
                admission,
            ),
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
            Err(reply) => self.reply(
                cfg,
                &message.channel_id,
                message.thread_ts.as_deref(),
                &reply,
                admission,
            ),
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
        if !self.output_failed.load(Ordering::Acquire)
            && self.local_effect_authority_is_current(admission)
        {
            self.post_message_traced(
                cfg,
                channel_id,
                text,
                thread_ts,
                admission.map(|context| context.trace),
            );
            if let Some(admission) = admission {
                admission.mark(AdmissionOutcome::LocalEffect);
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
                event_class = trace.event_class.as_str(),
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
                event_class = trace.event_class.as_str(),
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
                occurrence_key: received_message_key(message)
                    .expect("validated Slack message identity"),
                conversation: route,
                agent_id,
                sender: IngressSender {
                    user_id: message.user_id.clone(),
                    display_name: identity.display_name.clone(),
                    identity_alias: cfg.sender_aliases.get(&identity.user_id).cloned(),
                },
                report: IngressReport::Delivered {
                    message_id: slack_message_fact_id(
                        &message.channel_id,
                        message
                            .ts
                            .as_deref()
                            .or(message.event_id.as_deref())
                            .expect("validated Slack message identity"),
                    ),
                    text: text.to_owned(),
                },
                native_message_ts: message.ts.clone(),
            },
            admission,
        );
    }

    /// Submit one normalized Slack occurrence as a transient message report.
    fn submit_ingress(
        &self,
        cfg: &RuntimeConfig,
        submission: IngressSubmission,
        admission: Option<&AdmissionContext>,
    ) {
        let IngressSubmission {
            occurrence_key,
            conversation,
            agent_id,
            sender,
            report,
            native_message_ts,
        } = submission;
        if !self.admission_authority_is_current(admission) {
            log_ingress_rejection("stale_epoch");
            return;
        }
        let IngressSender {
            user_id,
            display_name,
            identity_alias,
        } = sender;
        let (publisher, installation_team_id) = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if admission.is_some_and(|context| !context.matches_state(&state))
                || self.shutdown.is_requested()
            {
                if let Some(admission) = admission {
                    admission.mark(AdmissionOutcome::StaleEpoch);
                }
                return;
            }
            if !state.registered_agents.contains(&agent_id)
                || !is_route_authorized(&state, cfg, &conversation, &user_id)
            {
                if let Some(admission) = admission {
                    admission.mark(AdmissionOutcome::RejectedRoute);
                }
                return;
            }
            let installation_team_id = admission
                .map(|context| context.installation_team_id.clone())
                .or_else(|| state.installation_team_id.clone());
            let Some(installation_team_id) = installation_team_id else {
                if let Some(admission) = admission {
                    admission.mark(AdmissionOutcome::RejectedRoute);
                }
                return;
            };
            let Some(instance_name) = state.instance_name.as_ref() else {
                if let Some(admission) = admission {
                    admission.mark(AdmissionOutcome::RejectedRoute);
                }
                return;
            };
            (
                tau_proto::RawMessagePublisherId::new(instance_name.to_string()),
                installation_team_id,
            )
        };
        let report_id = SlackReportId::from_occurrence(&occurrence_key);
        let extension_data = report_id.extension_data();
        let target = MessageAgentTarget::new(agent_id.to_string());
        let party = MessageParty {
            stable_id: slack_sender_ref(&installation_team_id, &user_id),
            display_name: identity_alias.clone().or_else(|| display_name.clone()),
            sender_auth: cfg.sender_policy(&user_id).map(|status| match status {
                SenderPolicyStatus::Allowlisted => MessageSenderAuth::VerifiedAllowlisted,
                SenderPolicyStatus::LaxPermitted => {
                    MessageSenderAuth::VerifiedConversationAuthorized
                }
            }),
        };
        let fact_conversation = Some(message_fact_conversation(&conversation));
        let (event, kind, reply_message_id, original_key, reaction_message_ts) = match report {
            IngressReport::Delivered { message_id, text } => {
                let mut fact = MessageDelivered::new(
                    publisher.clone(),
                    target,
                    message_id.clone(),
                    party,
                    fact_conversation,
                    text,
                );
                fact.extension_data = extension_data;
                (
                    Event::MessageDeliveredReported(fact),
                    PendingIngressKind::Delivered,
                    message_id.clone(),
                    native_message_ts
                        .as_ref()
                        .map(|native| PostedMessageKey::new(&conversation.channel_id, native)),
                    native_message_ts,
                )
            }
            IngressReport::Edited {
                target: message_id,
                text,
            } => {
                let mut fact = MessageEdited::new(
                    publisher.clone(),
                    target,
                    MessageFactRef {
                        publisher_extension_id: publisher.clone(),
                        message_id: message_id.clone(),
                    },
                    Some(party),
                    fact_conversation,
                    text,
                );
                fact.extension_data = extension_data;
                (
                    Event::MessageEditedReported(fact),
                    PendingIngressKind::Edited,
                    message_id,
                    None,
                    native_message_ts,
                )
            }
            IngressReport::ReactionAdded {
                target: message_id,
                reaction,
            } => {
                let mut fact = MessageReactionAdded::new(
                    publisher.clone(),
                    target,
                    MessageFactRef {
                        publisher_extension_id: publisher.clone(),
                        message_id: message_id.clone(),
                    },
                    Some(party),
                    fact_conversation,
                    reaction,
                );
                fact.extension_data = extension_data;
                (
                    Event::MessageReactionAddedReported(fact),
                    PendingIngressKind::ReactionAdded,
                    message_id,
                    None,
                    None,
                )
            }
            IngressReport::ReactionRemoved {
                target: message_id,
                reaction,
            } => {
                let mut fact = MessageReactionRemoved::new(
                    publisher.clone(),
                    target,
                    MessageFactRef {
                        publisher_extension_id: publisher,
                        message_id: message_id.clone(),
                    },
                    Some(party),
                    fact_conversation,
                    reaction,
                );
                fact.extension_data = extension_data;
                (
                    Event::MessageReactionRemovedReported(fact),
                    PendingIngressKind::ReactionRemoved,
                    message_id,
                    None,
                    None,
                )
            }
        };
        let pending_report = event.clone();
        let submission = self
            .output_submission_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if admission.is_some_and(|context| !context.matches_state(&state))
                || self.output_failed.load(Ordering::Acquire)
                || self.shutdown.is_requested()
                || !state.registered_agents.contains(&agent_id)
                || !is_route_authorized(&state, cfg, &conversation, &user_id)
                || state.installation_team_id.as_deref() != Some(installation_team_id.as_str())
                || state.pending_ingress.contains_key(&report_id)
            {
                return;
            }
            let message_authority = matches!(
                kind,
                PendingIngressKind::Delivered | PendingIngressKind::Edited
            )
            .then(|| PendingMessageAuthority {
                original_key,
                reply_route: ReplyRoute {
                    agent_id: agent_id.clone(),
                    conversation: conversation.clone(),
                    user_id: user_id.clone(),
                    display_name,
                    identity_alias,
                    installation_team_id: installation_team_id.clone(),
                },
                reaction_message_ts,
                conversation,
                user_id,
                installation_team_id,
            });
            let permit = admission.and_then(|context| context.permit.borrow_mut().take());
            let ingress_epoch = state.ingress_epoch;
            let config_generation = state.config_generation;
            let agent_generation = state.agent_generation;
            state.pending_ingress.insert(
                report_id.clone(),
                PendingIngress {
                    kind,
                    agent_id,
                    message_id: reply_message_id,
                    ingress_epoch,
                    config_generation,
                    agent_generation,
                    message_authority,
                    report: pending_report,
                    _permit: permit,
                },
            );
        }
        #[cfg(test)]
        run_blocking_test_hook(&self.test_hooks.ingress_submission_boundary);
        if self.output_failed.load(Ordering::Acquire) || self.shutdown.is_requested() {
            self.state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .pending_ingress
                .remove(&report_id);
            return;
        }
        let sent = self
            .output
            .send_confirmed(HarnessInputMessage::emit_with_persist(event, false));
        if !sent {
            self.state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .pending_ingress
                .remove(&report_id);
            self.output_failed.store(true, Ordering::Release);
            drop(submission);
            self.retire_after_output_failure();
            if let Some(admission) = admission {
                admission.mark(AdmissionOutcome::RejectedRoute);
            }
            return;
        }
        drop(submission);
        if let Some(admission) = admission {
            admission.mark(AdmissionOutcome::Submitted);
            let trace = admission.trace;
            tracing::trace!(
                target: LOG_TARGET,
                schema = LATENCY_SCHEMA,
                connection_generation = trace.connection_generation,
                trace_seq = trace.trace_seq,
                event_class = trace.event_class.as_str(),
                frame_to_submit_us = elapsed_us(admission.trace_received_at()),
                identity_us = admission.identity_us.get(),
                queue_wait_us = admission.queue_wait_us,
                output_outcome = "flushed",
                "slack.message_report.submitted"
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
/// the exact alias, kind, thread, sender, and owner.
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
    policy.is_some_and(|policy| policy.alias == route.alias && policy.kind == route.kind)
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
            notify: path_tokio_sync::Notify::new(),
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
    let runtime = match path_tokio_runtime::Builder::new_current_thread()
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
        let outcome = runtime.block_on(socket_worker_once(
            &ext,
            &cfg,
            startup.take(),
            &admission,
            connection_generation,
        ));
        match outcome {
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
                ext.report_worker_connection_failure_once(&message);
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
    while let Some((work, outstanding_permit)) = queue.pop() {
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
            event_class = trace.event_class.as_str(),
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
            outcome: Cell::new(AdmissionOutcome::RejectedPolicy),
            permit: RefCell::new(Some(outstanding_permit)),
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
                DecodedSlackEvent::Delete(delete) => {
                    ext.process_slack_delete_admitted(delete, Some(&context));
                }
            }))
        });
        let panicked = process_result.is_some_and(|result| result.is_err());
        let outcome = if panicked {
            AdmissionOutcome::RejectedPolicy
        } else if !ext.admission_authority_is_current(Some(&context)) {
            AdmissionOutcome::StaleEpoch
        } else {
            context.outcome.get()
        };
        tracing::trace!(
            target: LOG_TARGET,
            schema = LATENCY_SCHEMA,
            connection_generation = trace.connection_generation,
            trace_seq = trace.trace_seq,
            event_class = trace.event_class.as_str(),
            duration_us = elapsed_us(started_at),
            outcome = outcome.as_str(),
            "slack.ingress.admission_finished"
        );
        if panicked {
            tracing::warn!(
                target: LOG_TARGET,
                lifecycle = "degraded",
                failure = "admission_worker_panic",
                "Slack admission occurrence panicked; continuing ordered admission"
            );
            context.mark(AdmissionOutcome::RejectedPolicy);
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

/// Marks one WebSocket connection offline on every return path.
struct WorkerOnlineGuard<'a> {
    /// Shared worker lifecycle state owned by the extension process.
    state: &'a Mutex<State>,
}

impl Drop for WorkerOnlineGuard<'_> {
    fn drop(&mut self) {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .worker_online = false;
    }
}

async fn socket_worker_once(
    ext: &Extension,
    cfg: &RuntimeConfig,
    startup: Option<WorkerStartup>,
    admission: &Arc<AdmissionQueue<AdmissionWork>>,
    connection_generation: u64,
) -> Result<WorkerOutcome, String> {
    socket_worker_once_with_heartbeat(
        ext,
        cfg,
        startup,
        admission,
        connection_generation,
        SocketHeartbeat::default(),
    )
    .await
}

/// Run one Socket Mode connection with explicit heartbeat timing.
///
/// Production uses [`SocketHeartbeat::default`]; injectable timing keeps stale
/// connection regressions deterministic and fast.
async fn socket_worker_once_with_heartbeat(
    ext: &Extension,
    cfg: &RuntimeConfig,
    startup: Option<WorkerStartup>,
    admission: &Arc<AdmissionQueue<AdmissionWork>>,
    connection_generation: u64,
    heartbeat: SocketHeartbeat,
) -> Result<WorkerOutcome, String> {
    // ast-grep-ignore: debug-assert-expression-must-not-mutate
    debug_assert!(!heartbeat.ping_interval.is_zero());
    // ast-grep-ignore: debug-assert-expression-must-not-mutate
    debug_assert!(heartbeat.ping_interval < heartbeat.pong_timeout);
    let _online_guard = WorkerOnlineGuard {
        state: ext.state.as_ref(),
    };
    let ws_url = match startup {
        Some(startup) => {
            let _submission = ext
                .output_submission_gate
                .lock()
                .unwrap_or_else(|error| error.into_inner());
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
            let _submission = ext
                .output_submission_gate
                .lock()
                .unwrap_or_else(|error| error.into_inner());
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
    let (mut ws, _response) = tokio_tungstenite::connect_async_with_config(
        &ws_url,
        Some(socket_websocket_config()),
        false,
    )
    .await
    .map_err(|_| "Slack websocket connection failed".to_owned())?;
    tracing::info!(target: LOG_TARGET, lifecycle = "connected", "Slack Socket Mode connected");
    let connected_at = Instant::now();
    let mut hello_at = None;
    let heartbeat_started_at = path_tokio_time::Instant::now();
    let mut heartbeat_tick = tokio::time::interval_at(
        heartbeat_started_at + heartbeat.ping_interval,
        heartbeat.ping_interval,
    );
    heartbeat_tick.set_missed_tick_behavior(path_tokio_time::MissedTickBehavior::Skip);
    let pong_deadline = tokio::time::sleep_until(heartbeat_started_at + heartbeat.pong_timeout);
    tokio::pin!(pong_deadline);
    loop {
        let frame = tokio::select! {
            biased;
            () = ext.shutdown.wait() => {
                return Ok(WorkerOutcome::Shutdown);
            }
            () = &mut pong_deadline => {
                return Err(SOCKET_HEARTBEAT_TIMEOUT_ERROR.to_owned());
            }
            _ = heartbeat_tick.tick() => {
                if let Some(outcome) = await_socket_write(
                    &ext.shutdown,
                    pong_deadline.as_mut(),
                    ws.send(Message::Ping(Vec::new().into())),
                    "Slack websocket ping failed",
                )
                .await?
                {
                    return Ok(outcome);
                }
                continue;
            }
            frame = ws.next() => frame,
        };
        let Some(frame) = frame else {
            return Ok(WorkerOutcome::ReconnectNow);
        };
        let frame = frame.map_err(|_| "Slack websocket frame failed".to_owned())?;
        if matches!(&frame, Message::Pong(_)) {
            pong_deadline
                .as_mut()
                .reset(path_tokio_time::Instant::now() + heartbeat.pong_timeout);
        }
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
            event_class = EventClass::Unsupported.as_str(),
            frame_class = socket_frame_class(&frame),
            since_hello_us = elapsed_us(hello_at.unwrap_or(connected_at)),
            "slack.ws.frame_received"
        );
        if let Some(outcome) = handle_socket_frame(
            ext,
            &mut ws,
            frame,
            admission,
            timing,
            &mut hello_at,
            pong_deadline.as_mut(),
        )
        .await?
        {
            return Ok(outcome);
        }
    }
}

/// Await one WebSocket write while preserving shutdown and heartbeat bounds.
///
/// When shutdown or the Pong deadline wins, the caller returns and drops the
/// WebSocket rather than attempting another potentially blocked close write.
async fn await_socket_write<F, E>(
    shutdown: &ShutdownSignal,
    pong_deadline: Pin<&mut tokio::time::Sleep>,
    write: F,
    failure: &'static str,
) -> Result<Option<WorkerOutcome>, String>
where
    F: Future<Output = Result<(), E>>,
{
    tokio::pin!(write);
    tokio::select! {
        biased;
        () = shutdown.wait() => Ok(Some(WorkerOutcome::Shutdown)),
        () = pong_deadline => Err(SOCKET_HEARTBEAT_TIMEOUT_ERROR.to_owned()),
        result = &mut write => result
            .map(|()| None)
            .map_err(|_| failure.to_owned()),
    }
}

async fn handle_socket_frame(
    ext: &Extension,
    ws: &mut WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    frame: Message,
    admission: &Arc<AdmissionQueue<AdmissionWork>>,
    timing: SocketFrameTiming,
    hello_at: &mut Option<Instant>,
    pong_deadline: Pin<&mut tokio::time::Sleep>,
) -> Result<Option<WorkerOutcome>, String> {
    match frame {
        Message::Text(text) => {
            handle_socket_text_frame(
                ext,
                ws,
                text.as_str(),
                admission,
                timing,
                hello_at,
                pong_deadline,
            )
            .await
        }
        Message::Close(_) => Ok(Some(WorkerOutcome::ReconnectNow)),
        Message::Ping(payload) => {
            await_socket_write(
                &ext.shutdown,
                pong_deadline,
                ws.send(Message::Pong(payload)),
                "Slack websocket pong failed",
            )
            .await
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
    ws: &mut WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    text: &str,
    admission: &Arc<AdmissionQueue<AdmissionWork>>,
    timing: SocketFrameTiming,
    hello_at: &mut Option<Instant>,
    mut pong_deadline: Pin<&mut tokio::time::Sleep>,
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
            event_class = EventClass::Malformed.as_str(),
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
                EventClass::Malformed
            } else {
                EventClass::Unsupported
            }
        },
        DecodedSlackEvent::event_class,
    );
    tracing::trace!(
        target: LOG_TARGET,
        schema = LATENCY_SCHEMA,
        connection_generation,
        trace_seq,
        event_class = event_class.as_str(),
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
                    event_class = event_class.as_str(),
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
                    event_class = event_class.as_str(),
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
            event_class = event_class.as_str(),
            has_supported_event = supported_event,
            elapsed_us = elapsed_us(received_at),
            "slack.ws.ack_queued"
        );
        let ack_started = Instant::now();
        let write = send_socket_ack(ext, ws, envelope_id, pong_deadline.as_mut()).await;
        let result = match write {
            Ok(None) => finish_socket_ack(Ok(()), supported_event),
            Ok(Some(outcome)) => return Ok(Some(outcome)),
            Err(error) => finish_socket_ack(Err(error), supported_event),
        };
        tracing::trace!(
            target: LOG_TARGET,
            schema = LATENCY_SCHEMA,
            connection_generation,
            trace_seq,
            event_class = event_class.as_str(),
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
    ext: &Extension,
    ws: &mut WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    envelope_id: &str,
    pong_deadline: Pin<&mut tokio::time::Sleep>,
) -> Result<Option<WorkerOutcome>, String> {
    let ack = serde_json::json!({ "envelope_id": envelope_id }).to_string();
    await_socket_write(
        &ext.shutdown,
        pong_deadline,
        ws.send(Message::Text(ack.into())),
        "Slack websocket acknowledgement failed",
    )
    .await
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
    let event_id = match payload.get("event_id") {
        None => None,
        Some(serde_json::Value::String(value))
            if !value.is_empty()
                && value.len() <= MAX_EVENT_ID_BYTES
                && !value.chars().any(char::is_control) =>
        {
            Some(value.clone())
        }
        Some(_) => return None,
    };
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
        && event.get("subtype").and_then(|value| value.as_str()) == Some("message_deleted")
    {
        let previous = event.get("previous_message")?;
        let message_ts = event.get("deleted_ts")?.as_str()?;
        if previous.get("ts")?.as_str()? != message_ts {
            return None;
        }
        validate_slack_ts(message_ts).ok()?;
        let thread_ts = previous.get("thread_ts").and_then(|value| value.as_str());
        if let Some(thread_ts) = thread_ts {
            validate_slack_ts(thread_ts).ok()?;
        }
        return Some(DecodedSlackEvent::Delete(SlackDelete {
            event_id,
            channel_id: event.get("channel")?.as_str()?.to_owned(),
            message_ts: message_ts.to_owned(),
            thread_ts: thread_ts.map(str::to_owned),
        }));
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

fn run_with_client<R, W, C>(reader: R, writer: W, client: Arc<C>) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write + Send + 'static,
    C: SlackClient + ReactionClient,
{
    let slack_client: Arc<dyn SlackClient> = client.clone();
    let reaction_client: Arc<dyn ReactionClient> = client;
    run_with_clients_and_scheduler(
        reader,
        writer,
        slack_client,
        reaction_client,
        Arc::new(SystemSendScheduler),
    )
}

/// Run the protocol client with independently injected API boundaries.
fn run_with_clients_and_scheduler<R, W>(
    reader: R,
    writer: W,
    client: Arc<dyn SlackClient>,
    reaction_client: Arc<dyn ReactionClient>,
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
            let ext = Extension::new_with_clients_and_scheduler(
                client,
                reaction_client,
                handle,
                scheduler,
            );
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
    /// Shared Slack lifecycle and delivery state.
    state: Arc<Mutex<State>>,
    /// Shared sent/delete confirmed-submission and lifecycle/fatal-output
    /// retirement barrier.
    output_submission_gate: Arc<Mutex<()>>,
    /// Wakes delivery workers after authority retirement.
    wake: Arc<SendWake>,
    /// Early fail-closed protocol-output latch shared by every worker view.
    output_failed: Arc<AtomicBool>,
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
            output_submission_gate: Arc::clone(&extension.output_submission_gate),
            wake: Arc::clone(&extension.send_wake),
            output_failed: Arc::clone(&extension.output_failed),
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
                &retirement.output_submission_gate,
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
    output_submission_gate: &Mutex<()>,
    wake: &SendWake,
) {
    let _submission = output_submission_gate
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    {
        let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
        state.ingress_epoch = state.ingress_epoch.wrapping_add(1);
        state.pending_ingress.clear();
        state.clear_send_ledger();
    }
    wake.notify_lifecycle_change();
}

/// Enter the one fail-closed protocol-output retirement state before shutdown.
fn retire_after_output_failure(
    state: &Arc<Mutex<State>>,
    output_submission_gate: &Mutex<()>,
    wake: &SendWake,
    output_failed: &AtomicBool,
    shutdown: &ShutdownSignal,
) {
    output_failed.store(true, Ordering::Release);
    let _submission = output_submission_gate
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    {
        let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
        state.ingress_epoch = state.ingress_epoch.wrapping_add(1);
        state.agent_generation = state.agent_generation.wrapping_add(1);
        state.session_generation = state.session_generation.wrapping_add(1);
        state.session_active = false;
        state.registered_agents.clear();
        state.send_agent_generations.clear();
        state.selected_agent_by_route.clear();
        state.clear_reply_routes();
        state.clear_incoming_messages();
        state.pending_ingress.clear();
        state.clear_send_ledger();
        state.posted_messages.clear();
        state.reactions.clear();
    }
    wake.notify_lifecycle_change();
    shutdown.request();
}

struct SlackExtension;

impl TauExtension for SlackExtension {
    type State = SlackRuntime;

    fn name(&self) -> &'static str {
        "tau-ext-slack"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder
            .message_bridge()
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
                    Ok(tau_proto::ToolRegistrationDeclared {
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
                    Ok(tau_proto::ToolRegistrationDeclared {
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
                        "Reply through a Tau-issued Slack reply_to, or send proactively to an operator-configured alias, optionally discoverable with {conversations}. A successful result returns a message_ref usable with separately authorized {react}. Native Slack conversation and thread IDs are never accepted."
                    ));
                    Ok(tau_proto::ToolRegistrationDeclared {
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
                        "Add or remove one emoji reaction on an exact Tau-issued Slack message_ref, including refs returned by {send}. Channel IDs and timestamps are never accepted as separate route arguments; aliases, toggle, list, and discovery are also rejected."
                    ));
                    Ok(tau_proto::ToolRegistrationDeclared {
                        tool,
                        tool_group: Some(slack_tool_group()),
                        prompt_fragment: None,
                    })
                },
                handle_tool_invocation,
            )
            .on_raw_live(
                tau_proto::EventSelector::Exact(tau_proto::EventName::MESSAGE_DELIVERED),
                handle_live_event,
            )
            .on_raw_live(
                tau_proto::EventSelector::Exact(tau_proto::EventName::MESSAGE_EDITED),
                handle_live_event,
            )
            .on_raw_live(
                tau_proto::EventSelector::Exact(tau_proto::EventName::MESSAGE_DELETED),
                handle_live_event,
            )
            .on_raw_live(
                tau_proto::EventSelector::Exact(tau_proto::EventName::MESSAGE_REACTION_ADDED),
                handle_live_event,
            )
            .on_raw_live(
                tau_proto::EventSelector::Exact(tau_proto::EventName::MESSAGE_REACTION_REMOVED),
                handle_live_event,
            )
            .on_raw_live(
                tau_proto::EventSelector::Exact(tau_proto::EventName::MESSAGE_SENT),
                handle_live_event,
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
    let instance_name = Some(cx.instance_name().clone());
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
    Ok(())
}

/// Apply lifecycle output from the harness.
fn handle_output_message(
    message: &tau_proto::HarnessOutputMessage,
    runtime: &mut SlackRuntime,
    _handle: &ClientHandle,
) -> ClientResult<()> {
    apply_output_message(message, &runtime.ext);
    Ok(())
}

/// Retire Slack-local authority when the harness disconnects.
fn apply_output_message(message: &tau_proto::HarnessOutputMessage, ext: &Extension) {
    if matches!(message, tau_proto::HarnessOutputMessage::Disconnect(_)) {
        ext.retire_send_authority();
        ext.shutdown.request();
    }
}

fn handle_tool_invocation(cx: tau_client::ToolContext<'_, SlackRuntime>) -> ClientResult<()> {
    let local = cx.local_tool_name().clone();
    cx.state
        .ext
        .dispatch_scoped_tool(&local, cx.invoke().clone());
    Ok(())
}

fn handle_live_event(cx: tau_client::RawEventContext<'_, SlackRuntime>) -> ClientResult<()> {
    cx.state.ext.apply_live_event(cx.event());
    Ok(())
}

impl Extension {
    /// Apply one event delivered through the registered production live-event
    /// path.
    fn apply_live_event(&self, event: &Event) {
        if let Some((kind, publisher, agent_id, message_id, extension_data)) =
            canonical_ingress_ack_fields(event)
        {
            let _submission = self
                .output_submission_gate
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            self.state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .acknowledge_canonical_ingress(
                    kind,
                    publisher,
                    agent_id,
                    message_id,
                    extension_data,
                );
            return;
        }
        match event {
            Event::MessageSent(fact) => {
                self.state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .acknowledge_canonical_send(fact);
            }
            Event::AgentDisplayNameSet(name) => {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                state
                    .agent_labels
                    .insert(name.agent_id.clone(), name.display_name.clone());
            }
            Event::AgentStarted(started) => {
                if let Some(display_name) = started.display_name.clone() {
                    let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    state
                        .agent_labels
                        .insert(started.agent_id.clone(), display_name);
                }
            }
            Event::SessionStarted(_) => {
                {
                    let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                    state.ingress_epoch = state.ingress_epoch.wrapping_add(1);
                    state.session_generation = state.session_generation.wrapping_add(1);
                    state.session_active = true;
                }
                self.send_wake.notify_lifecycle_change();
            }
            Event::SessionAgentUnloaded(unloaded) => {
                self.unload_agent(&unloaded.agent_id);
            }
            Event::SessionShutdown(_) => {
                #[cfg(test)]
                announce_test_gate_attempt(&self.test_hooks.lifecycle_gate_attempt);
                let _submission = self
                    .output_submission_gate
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                state.ingress_epoch = state.ingress_epoch.wrapping_add(1);
                state.agent_generation = state.agent_generation.wrapping_add(1);
                state.session_generation = state.session_generation.wrapping_add(1);
                state.session_active = false;
                state.registered_agents.clear();
                state.send_agent_generations.clear();
                state.agent_labels.clear();
                state.selected_agent_by_route.clear();
                state.clear_reply_routes();
                state.clear_incoming_messages();
                state.pending_ingress.clear();
                state.clear_send_ledger();
                state.posted_messages.clear();
                state.reactions.clear();
                self.send_wake.notify_lifecycle_change();
            }
            _ => {}
        }
    }
}

/// Extract exact canonical ingress acknowledgement fields from one live event.
fn canonical_ingress_ack_fields(
    event: &Event,
) -> Option<(
    PendingIngressKind,
    &tau_proto::MessagePublisherId,
    &MessageAgentTarget,
    &MessageFactId,
    &MessageExtensionData,
)> {
    match event {
        Event::MessageDelivered(fact) => Some((
            PendingIngressKind::Delivered,
            &fact.publisher_extension_id,
            &fact.agent_id,
            &fact.message_id,
            &fact.extension_data,
        )),
        Event::MessageEdited(fact) => Some((
            PendingIngressKind::Edited,
            &fact.publisher_extension_id,
            &fact.agent_id,
            &fact.target.message_id,
            &fact.extension_data,
        )),
        Event::MessageDeleted(fact) => Some((
            PendingIngressKind::Deleted,
            &fact.publisher_extension_id,
            &fact.agent_id,
            &fact.target.message_id,
            &fact.extension_data,
        )),
        Event::MessageReactionAdded(fact) => Some((
            PendingIngressKind::ReactionAdded,
            &fact.publisher_extension_id,
            &fact.agent_id,
            &fact.target.message_id,
            &fact.extension_data,
        )),
        Event::MessageReactionRemoved(fact) => Some((
            PendingIngressKind::ReactionRemoved,
            &fact.publisher_extension_id,
            &fact.agent_id,
            &fact.target.message_id,
            &fact.extension_data,
        )),
        _ => None,
    }
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
            example_field("reply_to", example_text("slack-message:0123456789abcdef")),
        ]),
        note: Some(
            "reply_to is a Tau-issued fact selector, not a bearer capability or separate channel argument.".to_owned(),
        ),
        subcommand: None,
    }];
    ToolSpec {
        name: tau_proto::ToolName::new(SEND_TOOL_NAME),
        model_visible_name: Some(tau_proto::ToolName::new(SEND_TOOL_NAME)),
        description: Some(
            "Send to exactly one authenticated Slack reply route or operator-configured destination alias. Native Slack conversation and thread identifiers are never accepted as separate route arguments from the model."
                .to_owned(),
        ),
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "message": { "type": "string" },
                "reply_to": {
                    "type": "string",
                    "description": "Tau-issued selector from a locally submitted Slack message report; mutually exclusive with destination"
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
    let bytes = path_base64_engine::general_purpose::URL_SAFE_NO_PAD
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

/// Construct a terminal success with a structured model-facing result.
fn structured_tool_result(invoke: ToolStarted, result: CborValue) -> Event {
    let mut tool_result = successful_tool_result(&invoke, "");
    tool_result.result = result;
    Event::ToolResult(tool_result)
}

/// Construct one ordinary terminal successful tool result.
fn successful_tool_result(invoke: &ToolStarted, text: &str) -> ToolResult {
    ToolResult {
        presentation: Default::default(),
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
        // Slack arguments can contain externally influenced message text and
        // native-control sentinels; errors expose only the closed message.
        details: None,
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
    if 500 <= status_code {
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
        if 500 <= status {
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
