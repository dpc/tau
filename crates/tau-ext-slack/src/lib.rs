//! Personal Slack Socket Mode bridge extension for Tau agents.
//!
//! The extension declares logical `slack_register`, `slack_conversations`, and
//! `slack_send` tools,
//! which `ToolNameScope` maps to final per-instance wire names. Proactive
//! destination authorization follows `DESIGN-tau-ext-slack-proactive-sends`. It
//! is disabled by default, requires Slack token secrets plus a non-empty
//! allowlist, and treats Slack text as external untrusted prompt input.
//! Reply routing follows
//! `DESIGN-tau-ext-slack-canonical-reply-selectors`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::error::Error;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
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

mod posted_message_cache;

use posted_message_cache::{PostedMessageCache, PostedMessageKey, PostedMessageOwner};

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

/// Logical tool group name shared by all Slack bridge tools.
pub const TOOL_GROUP_NAME: &str = "slack";

/// Tag marking tools that register an agent with the Slack bridge.
pub const REGISTER_TOOL_TAG: &str = "slack:register";

/// Tag marking tools that disclose configured Slack conversation policy.
pub const CONVERSATIONS_TOOL_TAG: &str = "slack:discover";

/// Tag marking tools that send messages through the Slack bridge.
pub const SEND_TOOL_TAG: &str = "slack:send";

const DEFAULT_API_BASE: &str = "https://slack.com/api";
const DEFAULT_MAX_MESSAGE_BYTES: usize = 16 * 1024;
const MAX_MESSAGE_BYTES: usize = 128 * 1024;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const DUPLICATE_CACHE_SIZE: usize = 1024;
const POSTED_MESSAGE_CACHE_SIZE: usize = 1024;
const ROUTE_CORRELATION_LIMIT: usize = 1024;
const REPLY_ROUTE_LIMIT: usize = 1024;
const PENDING_SEND_LIMIT: usize = 1024;
const ACCEPTED_SEND_ATTEMPT_LIMIT: usize = 256;
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
    fn open_socket(&self, cfg: &RuntimeConfig) -> Result<String, String>;

    /// Return the bot user id from `auth.test` using the configured bot token.
    fn auth_test(&self, cfg: &RuntimeConfig) -> Result<String, String>;

    /// Return whether an allowlisted Slack user is a live human account.
    fn is_human_user(&self, cfg: &RuntimeConfig, user_id: &str) -> Result<bool, String>;

    /// Send a plain text message to one configured or linked Slack
    /// conversation.
    fn post_message(
        &self,
        cfg: &RuntimeConfig,
        channel_id: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<PostedMessage, String>;
}

/// Stable identity returned by Slack for one successfully posted message.
#[derive(Clone)]
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
    /// Ingress sender policy. Omission deliberately preserves strict behavior.
    security_mode: SecurityMode,
    /// Exact static receive/proactive conversation policy.
    conversations: Vec<RawConversationPolicy>,
    /// Optional bounded dynamic one-to-one DM discovery policy.
    dynamic_direct_messages: Option<DynamicDirectMessages>,
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

/// Stable disposition of a Slack-accepted send, retained to prevent reposting.
#[derive(Clone)]
enum AcceptedSendDisposition {
    /// Completion that must be resubmitted to preserve durable ordering.
    Completion(Box<CompleteTransportSendRequest>),
    /// Stable post-acceptance validation failure.
    Rejected(String),
}

/// Fingerprint and disposition for one Slack-accepted tool call.
#[derive(Clone)]
struct AcceptedSendAttempt {
    /// Agent that owned the accepted call.
    agent_id: AgentId,
    /// Exact arguments used by the accepted call.
    arguments: CborValue,
    /// Replay behavior after Slack has accepted the post.
    disposition: AcceptedSendDisposition,
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
            security_mode: self.security_mode,
            conversations,
            parent_receives,
            thread_receives,
            proactive_aliases,
            dynamic_direct_messages: self.dynamic_direct_messages,
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
    /// Policy classification established before submission.
    policy_status: SenderPolicyStatus,
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
    /// Sender policy retained for the eventual source-bound reply route.
    policy_status: SenderPolicyStatus,
    /// Native create identity to bind after durable commit, when applicable.
    original_key: Option<PostedMessageKey>,
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
    /// Bounded same-process record preventing a repeated call id from
    /// reposting.
    accepted_send_attempts: HashMap<tau_proto::ToolCallId, AcceptedSendAttempt>,
    /// Oldest-first bound for accepted send attempts.
    accepted_send_attempt_order: VecDeque<tau_proto::ToolCallId>,
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
    worker_started: bool,
    worker_online: bool,
    worker_startup_failure_reported: bool,
    /// Whether the current consecutive verified-human API failure episode was
    /// reported.
    identity_failure_reported: bool,
    bot_user_id: Option<String>,
    duplicate_events: DuplicateCache,
    /// Recent bridge-authored Slack posts and their owning agents.
    posted_messages: PostedMessageCache,
}

impl State {
    /// Insert or refresh one canonical route while evicting the oldest route.
    fn insert_reply_route(&mut self, message_id: MessageId, route: ReplyRoute) {
        self.reply_route_order.retain(|id| id != &message_id);
        self.reply_route_order.push_back(message_id.clone());
        self.reply_routes.insert(message_id, route);
        while self.reply_routes.len() > REPLY_ROUTE_LIMIT {
            if let Some(oldest) = self.reply_route_order.pop_front() {
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

    /// Remember a Slack-accepted call so identical delivery cannot repost it.
    fn remember_accepted_send(
        &mut self,
        invoke: &ToolStarted,
        disposition: AcceptedSendDisposition,
    ) {
        self.accepted_send_attempt_order
            .retain(|call_id| call_id != &invoke.call_id);
        self.accepted_send_attempt_order
            .push_back(invoke.call_id.clone());
        self.accepted_send_attempts.insert(
            invoke.call_id.clone(),
            AcceptedSendAttempt {
                agent_id: invoke.agent_id.clone(),
                arguments: invoke.arguments.clone(),
                disposition,
            },
        );
        while self.accepted_send_attempts.len() > ACCEPTED_SEND_ATTEMPT_LIMIT {
            if let Some(oldest) = self.accepted_send_attempt_order.pop_front() {
                self.accepted_send_attempts.remove(&oldest);
            }
        }
    }

    /// Clear all same-process outbound idempotency state.
    fn clear_accepted_send_attempts(&mut self) {
        self.accepted_send_attempts.clear();
        self.accepted_send_attempt_order.clear();
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

struct Extension {
    state: Arc<Mutex<State>>,
    client: Arc<dyn SlackClient>,
    output: Output,
    shutdown: Arc<ShutdownSignal>,
}

impl Extension {
    /// Create a Slack extension instance using a supplied client
    /// implementation.
    fn new(client: Arc<dyn SlackClient>, output: impl Into<Output>) -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            client,
            output: output.into(),
            shutdown: Arc::new(ShutdownSignal::new()),
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
        state.clear_accepted_send_attempts();
        state.registered_agents.clear();
        state.selected_agent_by_route.clear();
        state.pending_ingress.clear();
        state.clear_reply_routes();
        state.clear_incoming_messages();
        state.pending_posts.clear();
        state.posted_messages.clear();
        state.capability_active = false;
        state.pending_capability_request = None;
        state.duplicate_events = DuplicateCache::default();
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
        state.config = None;
        state.registered_agents.clear();
        state.selected_agent_by_route.clear();
        state.pending_ingress.clear();
        state.clear_reply_routes();
        state.clear_incoming_messages();
        state.pending_posts.clear();
        state.clear_accepted_send_attempts();
        state.capability_active = false;
        state.pending_capability_request = None;
        state.posted_messages.clear();
        state.linked_dms.clear();
        state.bot_user_id = None;
        state.duplicate_events = DuplicateCache::default();
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
            _ => Some(tool_error(invoke, "unknown slack tool".to_owned())),
        };
        if let Some(event) = event {
            self.output.emit(event);
        }
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
            state.registered_agents.insert(invoke.agent_id.clone());
            state
                .agent_labels
                .entry(invoke.agent_id.clone())
                .or_insert_with(|| invoke.agent_id.to_string());
            if let Some((cfg, startup, _)) = startup {
                self.start_worker_locked(&mut state, cfg, Some(startup));
            }
        } else {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.registered_agents.remove(&invoke.agent_id);
            state
                .selected_agent_by_route
                .retain(|_, agent| agent != &invoke.agent_id);
            state.remove_agent_reply_routes(&invoke.agent_id);
            state.remove_agent_incoming_messages(&invoke.agent_id);
            state
                .pending_ingress
                .retain(|_, pending| pending.agent_id != invoke.agent_id);
            state
                .pending_posts
                .retain(|_, post| post.agent_id != invoke.agent_id);
            state.posted_messages.remove_agent(&invoke.agent_id);
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
        state.config_frozen = true;
        if let Some(startup) = &startup {
            state.bot_user_id = Some(startup.bot_user_id.clone());
        }
        state.worker_started = true;
        state.worker_startup_failure_reported = false;
        let state_arc = Arc::clone(&self.state);
        let output = self.output.clone();
        let client = Arc::clone(&self.client);
        let shutdown = Arc::clone(&self.shutdown);
        std::thread::spawn(move || {
            socket_worker_loop(state_arc, client, output, cfg, startup, shutdown)
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
            self.output.emit(Event::HarnessNotice(HarnessNotice {
                kind: tau_proto::notice_kind::EXTENSION_NOTICE.to_owned(),
                message: bounded_text(&message, MAX_DIAGNOSTIC_BYTES),
                level: NoticeLevel::Warning,
                always_show: false,
            }));
        }
    }

    fn verified_human(&self, cfg: &RuntimeConfig, user_id: &str) -> bool {
        match self.client.is_human_user(cfg, user_id) {
            Ok(is_human) => {
                self.state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .identity_failure_reported = false;
                if !is_human {
                    log_ingress_rejection("sender_not_human");
                }
                is_human
            }
            Err(error) => {
                let should_report = {
                    let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    !std::mem::replace(&mut state.identity_failure_reported, true)
                };
                if should_report {
                    tracing::warn!(target: LOG_TARGET, rejection = "identity_api_failure", error = %sanitize_diagnostic(&error, cfg), "Slack ingress occurrence rejected; users.info verification degraded");
                    self.output.emit(Event::HarnessNotice(HarnessNotice {
                        kind: tau_proto::notice_kind::EXTENSION_NOTICE.to_owned(),
                        message: bounded_text(
                            &format!(
                                "Slack rejected one ingress occurrence because users.info verification failed (check users:read scope and app reinstall): {}",
                                sanitize_diagnostic(&error, cfg)
                            ),
                            MAX_DIAGNOSTIC_BYTES,
                        ),
                        level: NoticeLevel::Warning,
                        always_show: false,
                    }));
                }
                false
            }
        }
    }

    fn handle_send(&self, invoke: ToolStarted) -> Option<Event> {
        let send_tool = self.output.wire_tool_name(SEND_TOOL_NAME);
        {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if let Some(attempt) = state.accepted_send_attempts.get(&invoke.call_id) {
                if attempt.agent_id != invoke.agent_id || attempt.arguments != invoke.arguments {
                    return Some(tool_error(
                        invoke,
                        format!("{send_tool} call id was replayed with conflicting arguments"),
                    ));
                }
                match &attempt.disposition {
                    AcceptedSendDisposition::Completion(request) => {
                        self.output
                            .send(HarnessInputMessage::CompleteTransportSend(request.clone()));
                        return None;
                    }
                    AcceptedSendDisposition::Rejected(message) => {
                        return Some(tool_error(invoke, message.clone()));
                    }
                }
            }
        }
        if let Err(message) =
            validate_object_fields(&invoke.arguments, &["message", "reply_to", "destination"])
        {
            return Some(tool_error(invoke, message));
        }
        let message = match cbor_string_field(&invoke.arguments, "message") {
            Ok(message) => message,
            Err(message) => return Some(tool_error(invoke, message)),
        };
        let reply_to = cbor_optional_string_field(&invoke.arguments, "reply_to");
        let destination_alias = cbor_optional_string_field(&invoke.arguments, "destination");
        let (reply_to, destination_alias) = match (reply_to, destination_alias) {
            (Ok(Some(reply)), Ok(None)) => (Some(MessageId::new(reply)), None),
            (Ok(None), Ok(Some(alias))) => (None, Some(alias)),
            (Ok(Some(_)), Ok(Some(_))) | (Ok(None), Ok(None)) => {
                return Some(tool_error(
                    invoke,
                    format!("{send_tool} requires exactly one of `reply_to` or `destination`"),
                ));
            }
            (Err(message), _) | (_, Err(message)) => return Some(tool_error(invoke, message)),
        };
        if message.trim().is_empty() {
            return Some(tool_error(invoke, "`message` must not be empty".to_owned()));
        }
        let (cfg, route, authorization, endpoint, conversation, policy_status) = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let Some(cfg) = state.config.clone() else {
                return Some(tool_error(
                    invoke,
                    "slack extension is not configured".to_owned(),
                ));
            };
            if message.len() > cfg.max_message_bytes {
                return Some(tool_error(
                    invoke,
                    "`message` exceeds slack max_message_bytes".to_owned(),
                ));
            }
            if state.pending_posts.len() >= PENDING_SEND_LIMIT {
                return Some(tool_error(
                    invoke,
                    format!("{send_tool} has too many completions awaiting Tau; try again later"),
                ));
            }
            if !state.capability_active {
                return Some(tool_error(
                    invoke,
                    format!("{send_tool} transport capability is not active"),
                ));
            }
            if let Some(reply_to) = &reply_to {
                if !state.registered_agents.contains(&invoke.agent_id) {
                    let register = self.output.wire_tool_name(REGISTER_TOOL_NAME);
                    return Some(tool_error(
                        invoke,
                        format!("Slack reply requires {register}(enabled: true) first"),
                    ));
                }
                let Some(route) = state.reply_routes.get(reply_to).cloned() else {
                    return Some(tool_error(
                        invoke,
                        format!("{send_tool} reply_to is unknown or stale"),
                    ));
                };
                if route.agent_id != invoke.agent_id {
                    return Some(tool_error(
                        invoke,
                        format!("{send_tool} reply_to belongs to another agent"),
                    ));
                }
                if !is_route_authorized(&state, &cfg, &route.conversation, &route.user_id) {
                    return Some(tool_error(
                        invoke,
                        format!("{send_tool} originating conversation is no longer authorized"),
                    ));
                }
                let endpoint = external_endpoint_for_route(&route);
                let conversation = message_conversation(&route.conversation);
                let policy = route.policy_status;
                state.config_frozen = true;
                (
                    cfg,
                    route.conversation.clone(),
                    tau_proto::TransportSendAuthorization::Reply {
                        message_id: reply_to.clone(),
                    },
                    endpoint,
                    conversation,
                    policy,
                )
            } else {
                let alias = destination_alias.as_ref().expect("exclusive selector");
                if !valid_conversation_alias(alias) {
                    return Some(tool_error(
                        invoke,
                        format!("{send_tool} destination is unknown or unauthorized"),
                    ));
                }
                let Some(destination) = cfg
                    .proactive_aliases
                    .contains(alias)
                    .then(|| cfg.conversations.get(alias))
                    .flatten()
                else {
                    return Some(tool_error(
                        invoke,
                        format!("{send_tool} destination is unknown or unauthorized"),
                    ));
                };
                let route = SlackConversation {
                    channel_id: destination.conversation_id.clone(),
                    thread_ts: destination.thread_ts.clone(),
                    kind: destination.kind,
                    alias: destination.alias.clone(),
                };
                state.config_frozen = true;
                (
                    cfg.clone(),
                    route,
                    tau_proto::TransportSendAuthorization::ConfiguredDestination {
                        alias: alias.clone(),
                    },
                    send_destination_endpoint(destination),
                    send_destination_conversation(destination),
                    SenderPolicyStatus::Internal,
                )
            }
        };
        let text = format!("[{}] {message}", invoke.agent_id.as_ref());
        match self
            .client
            .post_message(&cfg, &route.channel_id, &text, route.thread_ts.as_deref())
        {
            Ok(posted) => {
                if posted.channel_id != route.channel_id {
                    self.state
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .remember_accepted_send(
                            &invoke,
                            AcceptedSendDisposition::Rejected(
                                "Slack returned a conflicting destination conversation".to_owned(),
                            ),
                        );
                    return Some(tool_error(
                        invoke,
                        "Slack returned a conflicting destination conversation".to_owned(),
                    ));
                }
                if posted.thread_ts.is_some() && posted.thread_ts != route.thread_ts {
                    self.state
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .remember_accepted_send(
                            &invoke,
                            AcceptedSendDisposition::Rejected(
                                "Slack returned conflicting thread metadata".to_owned(),
                            ),
                        );
                    return Some(tool_error(
                        invoke,
                        "Slack returned conflicting thread metadata".to_owned(),
                    ));
                }
                let request_id = format!("slack-send-{}", invoke.call_id.as_str());
                let operation = MessageOperation::Create {
                    payload: MessagePayload::Text {
                        text: text.clone(),
                        format: TextFormat::Plain,
                    },
                };
                let draft = transport_draft(
                    endpoint,
                    &route,
                    operation,
                    ExternalMessageIdentity {
                        event_id: None,
                        message_id: Some(posted.ts.clone()),
                        revision_id: None,
                        dedup_key: Some(format!("send:{}:{}", route.channel_id, posted.ts)),
                    },
                    policy_status,
                    self.output.wire_tool_name(SEND_TOOL_NAME),
                );
                let mut draft = draft;
                draft.conversation = Some(conversation);
                let tool_result = successful_tool_result(&invoke, "sent Slack message");
                let completion = Box::new(CompleteTransportSendRequest {
                    request_id: request_id.clone(),
                    call_id: invoke.call_id.clone(),
                    agent_id: invoke.agent_id.clone(),
                    in_reply_to: reply_to,
                    authorization,
                    draft,
                    acceptance: MessageTransportAcceptance::SubmittedToTransport,
                    tool_result,
                });
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                state.remember_accepted_send(
                    &invoke,
                    AcceptedSendDisposition::Completion(completion.clone()),
                );
                state.pending_posts.insert(
                    request_id.clone(),
                    PendingPostedMessage {
                        conversation: route.clone(),
                        posted: posted.clone(),
                        agent_id: invoke.agent_id.clone(),
                        invoke: invoke.clone(),
                    },
                );
                self.output
                    .send(HarnessInputMessage::CompleteTransportSend(completion));
                None
            }
            Err(message) => Some(tool_error(invoke, message)),
        }
    }

    /// Cache ownership using the authenticated outbound request conversation.
    ///
    /// Slack may omit thread metadata in its response. A present response
    /// thread must agree with the request or ownership is not cached.
    fn remember_posted_message(
        &self,
        conversation: SlackConversation,
        post: PostedMessage,
        agent_id: AgentId,
    ) {
        if post.thread_ts.is_some() && post.thread_ts != conversation.thread_ts {
            return;
        }
        let key = PostedMessageKey::new(&conversation.channel_id, &post.ts);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.posted_messages.insert(
            key,
            PostedMessageOwner {
                agent_id,
                thread_ts: conversation.thread_ts,
            },
        );
    }

    fn process_slack_reaction(&self, reaction: SlackReaction) {
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
        if !self.verified_human(&cfg, &reaction.user_id) {
            return;
        }
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
            route,
            agent_id,
            IngressSender {
                policy_status: cfg
                    .sender_policy(&reaction.user_id)
                    .expect("sender revalidated"),
                user_id: reaction.user_id,
            },
            MessageOperation::Reaction {
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
            ExternalMessageIdentity {
                event_id: reaction.event_id,
                message_id: Some(reaction.message_ts),
                revision_id: None,
                dedup_key: Some(dedup_key),
            },
        );
    }

    /// Route a validated edit only when its original committed create is known.
    ///
    /// This commit-confirmed ownership lookup and its fail-closed rejection
    /// path implement `DESIGN-tau-ext-slack-edit-ownership`.
    fn process_slack_edit(&self, edit: SlackEdit) {
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
        if !self.verified_human(&cfg, &edit.editor_user_id) {
            return;
        }
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
            owner.conversation,
            owner.agent_id,
            IngressSender {
                policy_status: cfg
                    .sender_policy(&edit.editor_user_id)
                    .expect("sender admitted"),
                user_id: edit.editor_user_id,
            },
            MessageOperation::Edit {
                target: MessageRef {
                    message_id: Some(owner.message_id),
                    external_message_id: Some(edit.message_ts.clone()),
                },
                payload: MessagePayload::Text {
                    text: text.to_owned(),
                    format: TextFormat::Plain,
                },
            },
            ExternalMessageIdentity {
                event_id: edit.event_id,
                message_id: Some(edit.message_ts),
                revision_id: Some(edit.revision_ts),
                dedup_key: Some(dedup_key),
            },
        );
    }

    fn process_slack_message(&self, mut message: SlackMessage) {
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
        if !self.verified_human(&cfg, &message.user_id) {
            return;
        }
        let leading_mention = self.has_leading_bot_mention(&message.text);
        let Some(mut text) = self.trimmed_message_text(&cfg, &message) else {
            return;
        };
        if leading_mention {
            text = self.strip_bot_mention(&text);
        }
        if text.is_empty() {
            if !self.insert_command_duplicate(&message) {
                return;
            }
            self.reply(
                &cfg,
                &message.channel_id,
                message.thread_ts.as_deref(),
                help_text(),
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
            && !self.insert_command_duplicate(&message)
        {
            log_ingress_rejection("command_dedup");
            return;
        }
        if self.rejects_unlinked_command(&cfg, &message, command) {
            return;
        }
        if self.handle_command(&cfg, &message, is_dm, command, rest) {
            return;
        }
        self.route_plain_text(&cfg, &message, &text);
    }

    /// Suppress retry side effects for bridge-local commands only.
    ///
    /// Routed occurrences deliberately bypass this cache because durable
    /// harness deduplication must observe reconnect/retry submissions.
    fn insert_command_duplicate(&self, message: &SlackMessage) -> bool {
        let key = message
            .ts
            .as_ref()
            .map(|ts| format!("local:{}:{ts}", message.channel_id))
            .or_else(|| message.event_id.clone());
        key.is_none_or(|key| {
            self.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .duplicate_events
                .insert_new(key)
        })
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

    fn trimmed_message_text(&self, cfg: &RuntimeConfig, message: &SlackMessage) -> Option<String> {
        let text = message.text.trim();
        if text.is_empty() {
            if !self.insert_command_duplicate(message) {
                return None;
            }
            self.reply(
                cfg,
                &message.channel_id,
                message.thread_ts.as_deref(),
                "Only text messages are supported by this Tau bridge.",
            );
            None
        } else if text.len() > cfg.max_message_bytes {
            if !self.insert_command_duplicate(message) {
                return None;
            }
            self.reply(
                cfg,
                &message.channel_id,
                message.thread_ts.as_deref(),
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
                self.handle_agents_command(cfg, message);
                true
            }
            Some("select" | "/select") => {
                self.handle_select_command(cfg, message, rest);
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
                    message.thread_ts.as_deref(),
                    "Unknown Slack command. Supported commands: start, agents, select, to.",
                );
                true
            }
            Some(_) | None => false,
        }
    }

    fn handle_start_command(&self, cfg: &RuntimeConfig, message: &SlackMessage, is_dm: bool) {
        if !is_dm {
            self.reply(
                cfg,
                &message.channel_id,
                message.thread_ts.as_deref(),
                "Dynamic Slack linking is available only in one-to-one DMs.",
            );
            return;
        }
        if static_parent_receive_covers_dm(cfg, &message.channel_id) {
            self.reply(
                cfg,
                &message.channel_id,
                message.thread_ts.as_deref(),
                help_text(),
            );
            return;
        }
        if static_receive_covers_dm(cfg, &message.channel_id) {
            self.reply(
                cfg,
                &message.channel_id,
                message.thread_ts.as_deref(),
                "This DM already has a fixed-thread receive policy; dynamic linking cannot broaden it.",
            );
            return;
        }
        if cfg.dynamic_direct_messages.is_none() {
            self.reply(
                cfg,
                &message.channel_id,
                message.thread_ts.as_deref(),
                "Dynamic Slack DMs are disabled; configure `dynamic_direct_messages` or a static DM route.",
            );
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = state.linked_dms.get(&message.channel_id)
            && existing.user_id != message.user_id
        {
            drop(state);
            self.reply(
                cfg,
                &message.channel_id,
                message.thread_ts.as_deref(),
                "This DM is already linked to another exact Slack user.",
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
        );
    }

    fn handle_agents_command(&self, cfg: &RuntimeConfig, message: &SlackMessage) {
        let reply = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            agents_text(&state)
        };
        self.reply(
            cfg,
            &message.channel_id,
            message.thread_ts.as_deref(),
            &reply,
        );
    }

    fn handle_select_command(&self, cfg: &RuntimeConfig, message: &SlackMessage, rest: &str) {
        if rest.trim().is_empty() {
            self.reply(
                cfg,
                &message.channel_id,
                message.thread_ts.as_deref(),
                "Usage: select <agent-id-or-prefix>",
            );
            return;
        }
        let reply = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
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
        );
    }

    fn handle_to_command(&self, cfg: &RuntimeConfig, message: &SlackMessage, rest: &str) {
        let (target, body) = split_first(rest);
        if target.is_empty() || body.trim().is_empty() {
            if !self.insert_command_duplicate(message) {
                return;
            }
            self.reply(
                cfg,
                &message.channel_id,
                message.thread_ts.as_deref(),
                "Usage: to <agent-id-or-prefix> <message>",
            );
            return;
        }
        match self.resolve_registered_agent(target) {
            Ok(agent_id) => self.route_text(message, agent_id, body.trim()),
            Err(reply) => {
                if self.insert_command_duplicate(message) {
                    self.reply(
                        cfg,
                        &message.channel_id,
                        message.thread_ts.as_deref(),
                        &reply,
                    );
                }
            }
        }
    }

    fn route_plain_text(&self, cfg: &RuntimeConfig, message: &SlackMessage, text: &str) {
        let route_key = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            current_route_key(&state, cfg, message)
        };
        let Some(route_key) = route_key else {
            log_ingress_rejection("route_changed");
            return;
        };
        match self.plain_text_target(&route_key) {
            Ok(agent_id) => self.route_text(message, agent_id, text),
            Err(reply) => {
                if self.insert_command_duplicate(message) {
                    self.reply(
                        cfg,
                        &message.channel_id,
                        message.thread_ts.as_deref(),
                        &reply,
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

    fn reply(&self, cfg: &RuntimeConfig, channel_id: &str, thread_ts: Option<&str>, text: &str) {
        let _ = self.client.post_message(cfg, channel_id, text, thread_ts);
    }

    fn route_text(&self, message: &SlackMessage, agent_id: AgentId, text: &str) {
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
            route,
            agent_id,
            IngressSender {
                policy_status: cfg
                    .sender_policy(&message.user_id)
                    .expect("sender admitted"),
                user_id: message.user_id.clone(),
            },
            MessageOperation::Create {
                payload: MessagePayload::Text {
                    text: text.to_owned(),
                    format: TextFormat::Plain,
                },
            },
            ExternalMessageIdentity {
                event_id: message
                    .ts
                    .is_none()
                    .then(|| message.event_id.clone())
                    .flatten(),
                message_id: message.ts.clone(),
                revision_id: None,
                dedup_key: Some(dedup_key),
            },
        );
    }

    /// Submit one normalized Slack occurrence through the canonical typed RPC.
    fn submit_ingress(
        &self,
        cfg: &RuntimeConfig,
        conversation: SlackConversation,
        agent_id: AgentId,
        sender: IngressSender,
        operation: MessageOperation,
        external_identity: ExternalMessageIdentity,
    ) {
        let IngressSender {
            user_id,
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
        let request_id = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if !state.registered_agents.contains(&agent_id)
                || !is_route_authorized(&state, cfg, &conversation, &user_id)
                || !state.capability_active
            {
                return;
            }
            if state.pending_ingress.len() >= ROUTE_CORRELATION_LIMIT {
                drop(state);
                self.reply(
                    cfg,
                    &conversation.channel_id,
                    conversation.thread_ts.as_deref(),
                    "Tau has too many pending Slack prompts; try again later.",
                );
                return;
            }
            state.next_route_id = state.next_route_id.wrapping_add(1);
            let request_id = format!("slack-in-{}", state.next_route_id);
            state.pending_ingress.insert(
                request_id.clone(),
                PendingIngress {
                    agent_id: agent_id.clone(),
                    conversation: conversation.clone(),
                    user_id: user_id.clone(),
                    policy_status,
                    original_key,
                },
            );
            request_id
        };
        self.output
            .send(HarnessInputMessage::TransportMessageIngress(Box::new(
                TransportMessageIngressRequest {
                    request_id,
                    target_agent_id: agent_id,
                    draft: transport_draft(
                        MessageEndpoint::External {
                            stable_id: Some(user_id),
                            display_name: None,
                            actor_kind: ExternalActorKind::Human,
                        },
                        &conversation,
                        operation,
                        external_identity,
                        policy_status,
                        self.output.wire_tool_name(SEND_TOOL_NAME),
                    ),
                },
            )));
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
        display_name: None,
        actor_kind: ExternalActorKind::Human,
    }
}

impl Drop for Extension {
    fn drop(&mut self) {
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
    state: Arc<Mutex<State>>,
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
        Err(error) => {
            tracing::warn!(target: LOG_TARGET, error = %error, "failed to create Slack worker runtime");
            return;
        }
    };
    let ext = Extension {
        state,
        client,
        output,
        shutdown: Arc::clone(&shutdown),
    };
    let mut backoff = INITIAL_RECONNECT_BACKOFF;
    let mut startup = startup;
    while !shutdown.is_requested() {
        match runtime.block_on(socket_worker_once(&ext, &cfg, startup.take())) {
            Ok(WorkerOutcome::ReconnectNow) => {
                tracing::warn!(target: LOG_TARGET, lifecycle = "reconnecting", "Slack Socket Mode connection ended; reconnecting");
                backoff = INITIAL_RECONNECT_BACKOFF;
            }
            Ok(WorkerOutcome::Shutdown) => break,
            Err(message) => {
                ext.report_worker_startup_failure_once(&cfg, &message);
                tracing::warn!(target: LOG_TARGET, lifecycle = "degraded", error = %sanitize_diagnostic(&message, &cfg), "Slack Socket Mode worker failed; reconnecting");
                if runtime.block_on(shutdown.wait_timeout(backoff)) {
                    break;
                }
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
    tracing::info!(target: LOG_TARGET, lifecycle = "connected", "Slack Socket Mode connected");
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
        tracing::warn!(target: LOG_TARGET, rejection = "oversized_frame", "dropping Slack Socket Mode frame");
        return Ok(None);
    }
    let action = handle_socket_text(ext, text);
    let ack_result = if let Some(envelope_id) = &action.ack_envelope_id {
        let supported_event = action.event.is_some();
        finish_socket_ack(send_socket_ack(cfg, ws, envelope_id).await, supported_event)
    } else {
        Ok(())
    };
    complete_socket_action(ext, action, ack_result)
}

/// Route a decoded action only after its required ACK succeeds.
fn complete_socket_action(
    ext: &Extension,
    action: SocketAction,
    ack_result: Result<(), String>,
) -> Result<Option<WorkerOutcome>, String> {
    ack_result?;
    let outcome = action.outcome();
    if let Some(event) = action.event {
        match event {
            DecodedSlackEvent::Message(message) => ext.process_slack_message(message),
            DecodedSlackEvent::Reaction(reaction) => ext.process_slack_reaction(reaction),
            DecodedSlackEvent::Edit(edit) => ext.process_slack_edit(edit),
        }
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
    /// Decoded supported event, when the envelope carries one.
    event: Option<DecodedSlackEvent>,
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
            action.event = decode_socket_event(&value);
        }
        _ => {}
    }
    action
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
    let state = tau_client::TauExtensionRunner::new(SlackExtension)
        .run_detached_writer_with_state(reader, writer, move |handle| SlackRuntime {
            ext: Extension::new(client, handle),
        })?;
    state.ext.shutdown.request();
    Ok(())
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
                    tool.description = Some(format!(
                        "Reply through an opaque Slack reply_to, or send proactively to an operator-configured alias, optionally discoverable with {conversations}. Native Slack conversation and thread IDs are never accepted."
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
        tau_proto::HarnessOutputMessage::RegisterTransportCapabilityResult(result) => {
            let mut state = ext.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.pending_capability_request.as_deref() == Some(result.request_id.as_str()) {
                state.pending_capability_request = None;
                state.capability_active = result.accepted;
            }
        }
        tau_proto::HarnessOutputMessage::TransportMessageIngressResult(result) => {
            let mut state = ext.state.lock().unwrap_or_else(|error| error.into_inner());
            let Some(pending) = state.pending_ingress.remove(&result.request_id) else {
                return;
            };
            if let (Some(message_id), Some(_)) = (&result.message_id, result.outcome) {
                if let Some(original_key) = pending.original_key.clone() {
                    state.insert_incoming_message(
                        original_key,
                        IncomingMessageOwner {
                            agent_id: pending.agent_id.clone(),
                            message_id: message_id.clone(),
                            conversation: pending.conversation.clone(),
                            user_id: pending.user_id.clone(),
                        },
                    );
                }
                state.insert_reply_route(
                    message_id.clone(),
                    ReplyRoute {
                        agent_id: pending.agent_id,
                        conversation: pending.conversation,
                        user_id: pending.user_id,
                        policy_status: pending.policy_status,
                    },
                );
            }
        }
        tau_proto::HarnessOutputMessage::CompleteTransportSendResult(result) => {
            let pending = ext
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .pending_posts
                .remove(&result.request_id);
            if let Some(pending) = pending {
                if result.accepted {
                    ext.remember_posted_message(
                        pending.conversation,
                        pending.posted,
                        pending.agent_id,
                    );
                } else {
                    ext.output.emit(tool_error(
                        pending.invoke,
                        format!(
                            "Slack accepted the post, but Tau rejected durable completion: {}",
                            result.error.as_deref().unwrap_or("completion_rejected")
                        ),
                    ));
                }
            }
        }
        _ => {}
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
            cx.state.ext.request_transport_capability();
        }
        Event::SessionAgentUnloaded(unloaded) => {
            let mut state = cx.state.ext.state.lock().unwrap_or_else(|e| e.into_inner());
            state.registered_agents.remove(&unloaded.agent_id);
            state.agent_labels.remove(&unloaded.agent_id);
            state
                .selected_agent_by_route
                .retain(|_, agent_id| agent_id != &unloaded.agent_id);
            state.remove_agent_reply_routes(&unloaded.agent_id);
            state.remove_agent_incoming_messages(&unloaded.agent_id);
            state
                .pending_ingress
                .retain(|_, pending| pending.agent_id != unloaded.agent_id);
            state
                .pending_posts
                .retain(|_, pending| pending.agent_id != unloaded.agent_id);
            state.posted_messages.remove_agent(&unloaded.agent_id);
        }
        Event::SessionShutdown(_) => {
            let mut state = cx.state.ext.state.lock().unwrap_or_else(|e| e.into_inner());
            state.registered_agents.clear();
            state.agent_labels.clear();
            state.selected_agent_by_route.clear();
            state.pending_ingress.clear();
            state.clear_reply_routes();
            state.clear_incoming_messages();
            state.pending_posts.clear();
            state.clear_accepted_send_attempts();
            state.capability_active = false;
            state.pending_capability_request = None;
            state.posted_messages.clear();
        }
        _ => {}
    }
    Ok(())
}

fn immutable_config_error() -> String {
    "slack configuration is frozen after successful Socket Mode preflight or an authorized Slack post attempt; restart Tau to apply new Slack settings"
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

    /// Call `users.info` with a form-encoded parameter.
    ///
    /// Slack accepts this method via GET/form encoding but treats a JSON POST
    /// body as if the required `user` argument were absent.
    fn get_user(&self, cfg: &RuntimeConfig, user_id: &str) -> Result<serde_json::Value, String> {
        let url = format!("{}/users.info", cfg.api_base);
        let mut response = self
            .agent
            .post(&url)
            .header("Authorization", &format!("Bearer {}", cfg.bot_token))
            .send_form([("user", user_id)])
            .map_err(|error| {
                sanitize_diagnostic(&format!("Slack transport error: {error}"), cfg)
            })?;
        let status = response.status();
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|e| sanitize_diagnostic(&format!("reading Slack response: {e}"), cfg))?;
        parse_slack_api_response(cfg, "users.info", status.as_u16(), None, &text)
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

    fn is_human_user(&self, cfg: &RuntimeConfig, user_id: &str) -> Result<bool, String> {
        let value = self.get_user(cfg, user_id)?;
        human_user_from_response(&value, user_id)
    }

    fn post_message(
        &self,
        cfg: &RuntimeConfig,
        channel_id: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<PostedMessage, String> {
        let body = post_message_body(channel_id, text, thread_ts);
        let value = self.post(cfg, "chat.postMessage", &cfg.bot_token, body)?;
        if value.get("channel").and_then(|value| value.as_str()) != Some(channel_id) {
            return Err("Slack returned a conflicting conversation".to_owned());
        }
        posted_message_from_response(&value)
    }
}

fn post_message_body(channel_id: &str, text: &str, thread_ts: Option<&str>) -> serde_json::Value {
    let mut body = serde_json::json!({ "channel": channel_id, "text": text });
    if let Some(thread_ts) = thread_ts {
        body["thread_ts"] = serde_json::Value::String(thread_ts.to_owned());
    }
    body
}

fn human_user_from_response(
    value: &serde_json::Value,
    expected_user_id: &str,
) -> Result<bool, String> {
    let user = value
        .get("user")
        .ok_or_else(|| "Slack users.info response missing user".to_owned())?;
    Ok(expected_user_id != "USLACKBOT"
        && user.get("id").and_then(|value| value.as_str()) == Some(expected_user_id)
        && user.get("deleted").and_then(|value| value.as_bool()) == Some(false)
        && user.get("is_bot").and_then(|value| value.as_bool()) == Some(false)
        && user.get("is_app_user").and_then(|value| value.as_bool()) == Some(false))
}

fn posted_message_from_response(value: &serde_json::Value) -> Result<PostedMessage, String> {
    let channel_id = value
        .get("channel")
        .and_then(|value| value.as_str())
        .filter(|channel| validate_slack_id("channel", channel).is_ok())
        .ok_or_else(|| "Slack chat.postMessage response missing channel".to_owned())?
        .to_owned();
    let ts = value
        .get("ts")
        .and_then(|value| value.as_str())
        .filter(|ts| validate_slack_ts(ts).is_ok())
        .ok_or_else(|| "Slack chat.postMessage response missing ts".to_owned())?
        .to_owned();
    let thread_ts = value
        .get("message")
        .and_then(|message| message.get("thread_ts"))
        .and_then(|value| value.as_str())
        .filter(|ts| validate_slack_ts(ts).is_ok())
        .map(str::to_owned);
    Ok(PostedMessage {
        channel_id,
        ts,
        thread_ts,
    })
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
