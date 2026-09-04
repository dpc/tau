//! Personal XMPP bridge extension for Tau agents.
//!
//! The extension declares logical `xmpp_register` and `xmpp_send` tools, which
//! `ToolNameScope` maps to final per-instance wire names. It is disabled by
//! default, uses a mandatory JID allowlist, and treats XMPP text as external
//! untrusted prompt input.
//! Allowlist matching and default-recipient validation follow
//! `SPEC-tau-ext-xmpp-allowlist-and-default-recipient`.
//! Its transport, routing, lifecycle, and trust boundaries are summarized in
//! `ARCH-tau-ext-xmpp`.

mod muc_presence_cache;
mod output;
mod registration_authority;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread as path_std_thread;
use std::thread::JoinHandle;
use std::time::Duration;

use futures_util::StreamExt;
#[cfg(test)]
use muc_presence_cache::MAX_WARNED_MUC_ROOMS;
use muc_presence_cache::{
    Admission, MAX_MUC_OCCUPANTS_PER_ROOM, MAX_MUC_OCCUPANTS_TOTAL, MucPresenceCache,
};
use output::Output;
#[cfg(test)]
pub(crate) use output::SATURATION_HOOK;
use rand::RngCore;
use registration_authority::{RegistrationAuthority, RegistrationLease};
use tau_client::{ClientError, ClientResult, ExtensionBuilder, ManualRuntimePoll, TauExtension};
use tau_proto::{
    AgentId, CborValue, Event, ExtensionName, MessageAgentTarget, MessageConversation,
    MessageDelivered, MessageFactId, MessageParty, MessageSenderAuth, MessageSent,
    RawMessagePublisherId, SessionId, ToolError, ToolExample, ToolProgress, ToolResult, ToolSpec,
    ToolStarted, ToolUseState, ToolUseStatus,
};
use tokio::{runtime as path_tokio_runtime, sync as path_tokio_sync, time as path_tokio_time};
use tokio_xmpp::rustls::crypto as path_tokio_xmpp_rustls_crypto;
use tokio_xmpp::{Client, IqRequest, IqResponse};
use xmpp_parsers::delay::Delay;
use xmpp_parsers::iq::Iq;
use xmpp_parsers::jid::{BareJid, Jid};
use xmpp_parsers::message::{Lang, Message, MessageType};
use xmpp_parsers::muc::muc::History;
use xmpp_parsers::muc::user::{Invite, Status as MucStatus};
use xmpp_parsers::muc::{Muc, MucUser};
use xmpp_parsers::presence::{Presence, Type as PresenceType};
use xmpp_parsers::stanza::Stanza;
use xmpp_parsers::stanza_error::StanzaError;
use xmpp_parsers::{minidom as path_xmpp_parsers_minidom, ns};

/// Tracing target used by this extension.
pub const LOG_TARGET: &str = "xmpp";

/// Logical tool name for registering the current agent as an XMPP listener.
pub const REGISTER_TOOL_NAME: &str = "xmpp_register";

/// Logical tool name for sending an XMPP message from a registered agent.
pub const SEND_TOOL_NAME: &str = "xmpp_send";

/// Logical tool group name shared by all XMPP bridge tools.
pub const TOOL_GROUP_NAME: &str = "xmpp";

/// Tag marking tools that register an agent with the XMPP bridge.
pub const REGISTER_TOOL_TAG: &str = "xmpp:register";

/// Tag marking tools that send messages through the XMPP bridge.
pub const SEND_TOOL_TAG: &str = "xmpp:send";

const DEFAULT_RESOURCE_PREFIX: &str = "tau";
const DEFAULT_ROOM_PREFIX: &str = "tau";
const DEFAULT_MESSAGE_LIMIT: usize = 16 * 1024;
const MAX_MESSAGE_LIMIT: usize = 128 * 1024;
/// Maximum UTF-8 size of one outbound message body.
///
/// The reported truncation occurred at 4096 Unicode scalar values. Tau uses a
/// conservative 4096-byte policy so multibyte text cannot cross that observed
/// display boundary, and visibly numbers messages that require multiple
/// stanzas.
const OUTBOUND_BODY_LIMIT_BYTES: usize = 4096;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const REGISTER_TIMEOUT: Duration = Duration::from_secs(45);
// Keep command readiness semantics aligned with
// `SPEC-tau-ext-xmpp-readiness-waits`.
const ONLINE_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const READY_RESPONSE_SLACK: Duration = Duration::from_secs(1);
const STANZA_TIMEOUT: Duration = Duration::from_secs(20);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const WORKER_SHUTDOWN_CLEANUP_TIMEOUT: Duration = Duration::from_secs(4);
const MUC_OWNER_NS: &str = "http://jabber.org/protocol/muc#owner";
const MUC_ROOM_DISAMBIGUATOR_BYTES: usize = 5;
const DEFAULT_ROOM_TEMPLATE: &str = "{{agent_id}}-{{agent_hash}}";

/// Run the XMPP extension over stdio.
pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    tau_client::init_logging_for(LOG_TARGET);
    run(std::io::stdin(), std::io::stdout())
}

/// Run the XMPP extension over an arbitrary transport.
pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    run_with_bridge(reader, writer, Arc::new(LiveXmppBridge::default()))
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
            notify: path_tokio_sync::Notify::new(),
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
        authority: Arc<RegistrationAuthority>,
    ) -> Result<(), String>;

    /// Register one agent conversation and return its XMPP address.
    fn register_agent(
        &self,
        cfg: &RuntimeConfig,
        agent_id: &AgentId,
        lease: RegistrationLease,
        room_localpart: Option<&str>,
    ) -> Result<String, String>;

    /// Enqueue best-effort remote cleanup for one exact registration lease.
    fn unregister_agent(&self, agent_id: &AgentId, lease: RegistrationLease) -> Result<(), String>;

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
    /// Resolved account password. Never log or expose this value outside the
    /// final XMPP client adapter.
    password: tau_proto::SecretValue,
    /// JIDs allowed to deliver messages through this bridge.
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
    /// Harness-configured instance identity used for publisher claims and
    /// generated resources/rooms; present in every admitted runtime config.
    instance_name: ExtensionName,
}

/// Raw deserialized extension config from `harness.yaml`.
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ExtConfig {
    /// Bare XMPP account JID used for login.
    jid: Option<String>,
    /// Secret name carrying the XMPP password.
    password_secret: Option<String>,
    /// JIDs allowed to deliver messages.
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
    /// Handlebars template rendering the complete room localpart.
    room_template: Option<String>,
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
    /// Handlebars template rendering the complete room localpart.
    room_template: String,
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
        instance_name: ExtensionName,
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
        let room_prefix = validate_room_prefix(self.room_prefix);
        let room_template =
            validate_room_template(self.room_template, &room_prefix, service.as_ref())?;
        Ok(MucConfig {
            service,
            room_prefix,
            room_template,
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
) -> Result<tau_proto::SecretValue, String> {
    let secret_name =
        password_secret.ok_or_else(|| "xmpp config requires `password_secret`".to_owned())?;
    secrets
        .get(&secret_name)
        .filter(|password| !password.expose_secret().trim().is_empty())
        .cloned()
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
    if MAX_MESSAGE_LIMIT < max_message_bytes {
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

/// Values available while rendering one configured MUC room localpart.
#[derive(serde::Serialize)]
struct RoomTemplateContext<'a> {
    /// Full durable Tau agent id.
    agent_id: &'a str,
    /// Stable eight-character hash over the full agent id.
    agent_hash: String,
    /// Current Tau session id.
    session_id: &'a str,
    /// Agent role id, or an empty string when its creation fact is unavailable.
    role: &'a str,
    /// Alias of [`Self::role`] that names its identifier semantics explicitly.
    role_id: &'a str,
    /// Whether [`Self::role`] is available.
    role_present: bool,
    /// Alias of [`Self::role_present`].
    role_id_present: bool,
    /// Agent role-group id, or an empty string when role metadata is
    /// unavailable.
    role_group: &'a str,
    /// Alias of [`Self::role_group`] using the user's group-id terminology.
    group_id: &'a str,
    /// Whether [`Self::role_group`] is available.
    role_group_present: bool,
    /// Alias of [`Self::role_group_present`].
    group_id_present: bool,
    /// Validated legacy room prefix.
    room_prefix: &'a str,
    /// Configured extension instance name.
    instance_name: &'a str,
    /// Whether [`Self::instance_name`] is available; always true for configured
    /// runtimes.
    instance_name_present: bool,
}

/// Handlebars helper providing explicitly opt-in random room-name segments.
struct RandomAlphanumericHelper;

impl handlebars::HelperDef for RandomAlphanumericHelper {
    fn call_inner<'reg: 'rc, 'rc>(
        &self,
        helper: &handlebars::Helper<'rc>,
        _: &'reg handlebars::Handlebars<'reg>,
        _: &'rc handlebars::Context,
        _: &mut handlebars::RenderContext<'reg, 'rc>,
    ) -> Result<handlebars::ScopedJson<'rc>, handlebars::RenderError> {
        if helper.params().len() != 1 || !helper.hash().is_empty() {
            return Err(handlebars::RenderErrorReason::Other(
                "random_alphanumeric requires exactly one length argument".to_owned(),
            )
            .into());
        }
        let len = helper
            .param(0)
            .and_then(|param| param.value().as_u64())
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| (1..=64).contains(value))
            .ok_or_else(|| {
                handlebars::RenderErrorReason::Other(
                    "random_alphanumeric length must be an integer from 1 through 64".to_owned(),
                )
            })?;
        use rand::Rng as _;
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        let mut rng = rand::thread_rng();
        let value = (0..len)
            .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
            .collect::<String>();
        Ok(handlebars::ScopedJson::Derived(serde_json::Value::String(
            value,
        )))
    }
}

fn room_template_engine() -> handlebars::Handlebars<'static> {
    let mut handlebars = handlebars::Handlebars::new();
    handlebars.set_strict_mode(true);
    handlebars.register_escape_fn(handlebars::no_escape);
    handlebars.register_helper("random_alphanumeric", Box::new(RandomAlphanumericHelper));
    handlebars
}

fn validate_room_template(
    template: Option<String>,
    room_prefix: &str,
    service: Option<&BareJid>,
) -> Result<String, String> {
    let template = template.unwrap_or_else(|| DEFAULT_ROOM_TEMPLATE.to_owned());
    if template.trim().is_empty() {
        return Err("xmpp muc.room_template must not be empty".to_owned());
    }
    let sample = RoomTemplateContext {
        agent_id: "agent-A1b2",
        agent_hash: "0123abcd".to_owned(),
        session_id: "session-1",
        role: "engineer",
        role_id: "engineer",
        role_present: true,
        role_id_present: true,
        role_group: "engineering",
        group_id: "engineering",
        role_group_present: true,
        group_id_present: true,
        room_prefix,
        instance_name: "default",
        instance_name_present: true,
    };
    let rendered = render_room_template(&template, &sample)?;
    let missing_metadata = RoomTemplateContext {
        agent_id: "agent-A1b2",
        agent_hash: "0123abcd".to_owned(),
        session_id: "session-1",
        role: "",
        role_id: "",
        role_present: false,
        role_id_present: false,
        role_group: "",
        group_id: "",
        role_group_present: false,
        group_id_present: false,
        room_prefix,
        instance_name: "",
        instance_name_present: false,
    };
    render_room_template(&template, &missing_metadata)?;
    let domain = service
        .map(|service| service.domain().to_string())
        .unwrap_or_else(|| "conference.invalid".to_owned());
    validate_rendered_room_localpart(&rendered, &domain)?;
    Ok(template)
}

fn render_room_template(
    template: &str,
    context: &RoomTemplateContext<'_>,
) -> Result<String, String> {
    room_template_engine()
        .render_template(template, context)
        .map_err(|error| format!("xmpp muc.room_template failed to render: {error}"))
}

fn validate_rendered_room_localpart(localpart: &str, domain: &str) -> Result<(), String> {
    if localpart.is_empty() {
        return Err("xmpp muc.room_template rendered an empty room localpart".to_owned());
    }
    let jid = Jid::new(&format!("{localpart}@{domain}")).map_err(|error| {
        format!("xmpp muc.room_template rendered invalid room localpart `{localpart}`: {error}")
    })?;
    if jid.node().is_none() || jid.resource().is_some() || jid.domain().as_str() != domain {
        return Err(format!(
            "xmpp muc.room_template must render exactly one bare room localpart, got `{localpart}`"
        ));
    }
    Ok(())
}

#[derive(Default)]
struct State {
    /// Validated runtime config.
    config: Option<RuntimeConfig>,
    /// Agents currently registered with the bridge.
    registered_agents: HashMap<AgentId, RegistrationLease>,
    /// XMPP conversation address per agent.
    conversations: HashMap<AgentId, String>,
    /// Immutable Tau session id used to scope registration lifecycle cleanup.
    current_session_id: Option<SessionId>,
    /// Durable role observed for each agent.
    agent_roles: HashMap<AgentId, String>,
    /// Memory-only agents whose cached roles may be dropped on unload/shutdown.
    ephemeral_agent_roles: HashSet<AgentId>,
    /// Available role-to-group mapping announced by the harness.
    role_groups: HashMap<String, String>,
    /// Whether the XMPP bridge has been started.
    bridge_started: bool,
}

fn room_localpart_for_registration(
    state: &State,
    cfg: &RuntimeConfig,
    session_id: &SessionId,
    agent_id: &AgentId,
) -> Result<Option<String>, String> {
    if cfg.routing_mode != RoutingMode::Muc {
        return Ok(None);
    }
    let role = state.agent_roles.get(agent_id);
    let group = role.and_then(|role| state.role_groups.get(role));
    let context = RoomTemplateContext {
        agent_id: agent_id.as_ref(),
        agent_hash: muc_room_disambiguator(agent_id),
        session_id: session_id.as_ref(),
        role: role.map(String::as_str).unwrap_or(""),
        role_id: role.map(String::as_str).unwrap_or(""),
        role_present: role.is_some(),
        role_id_present: role.is_some(),
        role_group: group.map(String::as_str).unwrap_or(""),
        group_id: group.map(String::as_str).unwrap_or(""),
        role_group_present: group.is_some(),
        group_id_present: group.is_some(),
        room_prefix: &cfg.muc.room_prefix,
        instance_name: cfg.instance_name.as_str(),
        instance_name_present: true,
    };
    let localpart = render_room_template(&cfg.muc.room_template, &context)?;
    let service = cfg
        .muc
        .service
        .as_ref()
        .ok_or_else(|| "xmpp muc.service is not configured".to_owned())?;
    validate_rendered_room_localpart(&localpart, service.domain().as_str())?;
    Ok(Some(localpart))
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
    /// Generation-bound authority shared with worker inbound routing.
    authority: Arc<RegistrationAuthority>,
}

impl Extension {
    fn new(bridge: Arc<dyn XmppBridge>, output: impl Into<Output>) -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            bridge,
            output: output.into(),
            shutdown: Arc::new(ShutdownSignal::new()),
            authority: Arc::new(RegistrationAuthority::default()),
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
            self.authority.revoke_all();
        }
    }

    /// Revoke one agent's local authority before enqueueing remote cleanup.
    fn revoke_agent(&self, agent_id: &AgentId) {
        let lease = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.registered_agents.remove(agent_id);
            state.conversations.remove(agent_id);
            self.authority.revoke_current(agent_id)
        };
        if let Some(lease) = lease {
            let _ = self.bridge.unregister_agent(agent_id, lease);
        }
    }

    /// Revoke all local authority before enqueueing remote cleanup.
    fn revoke_all(&self) {
        let leases = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.registered_agents.clear();
            state.conversations.clear();
            self.authority.revoke_all()
        };
        for (agent_id, lease) in leases {
            let _ = self.bridge.unregister_agent(&agent_id, lease);
        }
    }

    fn dispatch_scoped_tool(
        &self,
        local_tool_name: &tau_proto::ToolName,
        invoke: ToolStarted,
    ) -> ClientResult<()> {
        self.output.report_tool_progress(ToolProgress {
            call_id: invoke.call_id.clone(),
            tool_name: invoke.tool_name.clone(),
            message: Some("xmpp tool started".to_owned()),
            progress: None,
            display: Some(ToolUseState {
                status: ToolUseStatus::InProgress,
                status_text: tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
                ..Default::default()
            }),
        });
        let event = match local_tool_name.as_str() {
            REGISTER_TOOL_NAME => self.handle_register(invoke),
            SEND_TOOL_NAME => self.handle_send(invoke),
            _ => tool_error(invoke, "unknown xmpp tool".to_owned()),
        };
        self.output.check_mandatory_output()?;
        self.output.report_tool_terminal(event)
    }

    #[cfg(test)]
    fn dispatch_tool(&self, invoke: ToolStarted) {
        let local = invoke.tool_name.clone();
        self.dispatch_scoped_tool(&local, invoke)
            .expect("test output channel remains connected");
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
            let (cfg, room_localpart, lease) = {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                let Some(cfg) = state.config.clone() else {
                    return tool_error(invoke, "xmpp extension is not configured".to_owned());
                };
                let Some(session_id) = state.current_session_id.clone() else {
                    return tool_error(
                        invoke,
                        "The XMPP registration tool requires an active Tau session; no session_started event has been observed yet".to_owned(),
                    );
                };
                let room_localpart = match room_localpart_for_registration(
                    &state,
                    &cfg,
                    &session_id,
                    &invoke.agent_id,
                ) {
                    Ok(localpart) => localpart,
                    Err(message) => return tool_error(invoke, message),
                };
                if !state.bridge_started {
                    if let Err(message) = self.bridge.ensure_started(
                        cfg.clone(),
                        self.output.clone(),
                        Arc::clone(&self.shutdown),
                        Arc::clone(&self.authority),
                    ) {
                        return tool_error(invoke, message);
                    }
                    state.bridge_started = true;
                }
                let lease = self.authority.reserve(invoke.agent_id.clone());
                (cfg, room_localpart, lease)
            };
            if let Err(message) = self.bridge.wait_until_ready(ONLINE_WAIT_TIMEOUT) {
                self.authority.revoke(&invoke.agent_id, lease);
                return tool_error(invoke, message);
            }
            let address = match self.bridge.register_agent(
                &cfg,
                &invoke.agent_id,
                lease,
                room_localpart.as_deref(),
            ) {
                Ok(address) => address,
                Err(message) => {
                    self.authority.revoke(&invoke.agent_id, lease);
                    return tool_error(invoke, message);
                }
            };
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if !self.authority.activate(&invoke.agent_id, lease) {
                drop(state);
                let _ = self.bridge.unregister_agent(&invoke.agent_id, lease);
                return tool_error(
                    invoke,
                    "xmpp registration was revoked before completion".to_owned(),
                );
            }
            state
                .registered_agents
                .insert(invoke.agent_id.clone(), lease);
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
            self.revoke_agent(&invoke.agent_id);
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
            if !state.registered_agents.contains_key(&invoke.agent_id) {
                return tool_error(
                    invoke,
                    "The XMPP send tool requires the registration tool with enabled=true first"
                        .to_owned(),
                );
            }
            let Some(registered_conversation) = state.conversations.get(&invoke.agent_id).cloned()
            else {
                return tool_error(
                    invoke,
                    "The XMPP send tool requires a live registered conversation".to_owned(),
                );
            };
            let conversation = match cfg.routing_mode {
                RoutingMode::Muc => registered_conversation,
                RoutingMode::DirectResource => cfg.default_recipient.to_bare().to_string(),
            };
            let publisher = RawMessagePublisherId::new(cfg.instance_name.as_str());
            drop(state);
            let parts = match outbound_message_parts(&invoke.agent_id, &message) {
                Ok(parts) => parts,
                Err(error) => return tool_error(invoke, error),
            };
            for (index, text) in parts.iter().enumerate() {
                if let Err(error) = self.bridge.send_message(&invoke.agent_id, text) {
                    let error = if parts.len() == 1 {
                        error
                    } else {
                        format!(
                            "failed to send XMPP message part {}/{} after {} complete part(s): {error}",
                            index + 1,
                            parts.len(),
                            index
                        )
                    };
                    return tool_error(invoke, error);
                }
            }
            let _ = self
                .output
                .emit_message_report(Event::MessageSentReported(MessageSent::new(
                    publisher,
                    MessageAgentTarget::new(invoke.agent_id.as_ref()),
                    generated_xmpp_send_message_id(invoke.call_id.as_str(), &conversation),
                    None,
                    Some(MessageConversation {
                        stable_id: conversation,
                        display_name: None,
                        alias: None,
                    }),
                    &message,
                )));
            tool_result(invoke, "sent XMPP message")
        }
    }
}

/// Format an outbound tool message into UTF-8-safe, visibly numbered bodies.
fn outbound_message_parts(agent_id: &AgentId, message: &str) -> Result<Vec<String>, String> {
    let prefix = format!("[{}] ", agent_id.as_ref());
    if prefix.len() + message.len() <= OUTBOUND_BODY_LIMIT_BYTES {
        return Ok(vec![format!("{prefix}{message}")]);
    }

    let mut expected_parts = 2;
    loop {
        let mut parts = Vec::with_capacity(expected_parts);
        let mut remaining = message;
        while !remaining.is_empty() {
            let marker = format!("[part {}/{}] ", parts.len() + 1, expected_parts);
            let overhead = prefix.len() + marker.len();
            let Some(capacity) = OUTBOUND_BODY_LIMIT_BYTES.checked_sub(overhead) else {
                return Err(format!(
                    "XMPP agent prefix and multipart marker exceed the {OUTBOUND_BODY_LIMIT_BYTES}-byte outbound body limit"
                ));
            };
            if capacity == 0 {
                return Err(format!(
                    "XMPP agent prefix and multipart marker leave no payload space within the {OUTBOUND_BODY_LIMIT_BYTES}-byte outbound body limit"
                ));
            }
            let mut end = remaining.len().min(capacity);
            while !remaining.is_char_boundary(end) {
                end -= 1;
            }
            if end == 0 {
                return Err(format!(
                    "XMPP multipart payload cannot fit one UTF-8 character within the {OUTBOUND_BODY_LIMIT_BYTES}-byte outbound body limit"
                ));
            }
            let (chunk, rest) = remaining.split_at(end);
            parts.push(format!("{prefix}{marker}{chunk}"));
            remaining = rest;
        }
        if parts.len() == expected_parts {
            return Ok(parts);
        }
        // The denominator can widen its own marker at a power-of-ten boundary.
        // Re-split until the advertised and actual counts reach a fixed point.
        expected_parts = parts.len();
    }
}

impl Drop for Extension {
    fn drop(&mut self) {
        self.revoke_all();
        self.shutdown.request();
        if let Err(_error) = self.bridge.shutdown(SHUTDOWN_TIMEOUT) {
            tracing::warn!(target: LOG_TARGET, "xmpp bridge shutdown did not finish cleanly");
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
        /// Agent to register.
        agent_id: AgentId,
        /// Exact registration generation being installed.
        lease: RegistrationLease,
        /// Rendered MUC room localpart, absent in direct-resource mode.
        room_localpart: Option<String>,
        /// Response channel carrying the conversation address.
        response: mpsc::Sender<Result<String, String>>,
    },
    Unregister {
        /// Agent to unregister.
        agent_id: AgentId,
        /// Exact stale generation eligible for cleanup.
        lease: RegistrationLease,
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
        authority: Arc<RegistrationAuthority>,
    ) -> Result<(), String> {
        let mut guard = self.command_tx.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_some() {
            return Ok(());
        }
        let (command_tx, command_rx) = mpsc::channel();
        let worker_tx = command_tx.clone();
        let (done_tx, done_rx) = mpsc::channel();
        let worker_shutdown = Arc::clone(&shutdown);
        let join = path_std_thread::Builder::new()
            .name("tau-ext-xmpp".to_owned())
            .spawn(move || {
                xmpp_thread(cfg, command_rx, output, worker_shutdown, authority);
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
        agent_id: &AgentId,
        lease: RegistrationLease,
        room_localpart: Option<&str>,
    ) -> Result<String, String> {
        let tx = self.command_sender()?;
        let (response_tx, response_rx) = mpsc::channel();
        tx.send(XmppCommand::Register {
            agent_id: agent_id.clone(),
            lease,
            room_localpart: room_localpart.map(ToOwned::to_owned),
            response: response_tx,
        })
        .map_err(|_| "xmpp worker is not running".to_owned())?;
        response_rx
            .recv_timeout(COMMAND_TIMEOUT)
            .map_err(|_| "timed out waiting for xmpp registration".to_owned())?
    }

    fn unregister_agent(&self, agent_id: &AgentId, lease: RegistrationLease) -> Result<(), String> {
        let tx = self.command_sender()?;
        tx.send(XmppCommand::Unregister {
            agent_id: agent_id.clone(),
            lease,
        })
        .map_err(|_| "xmpp worker is not running".to_owned())
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
    authority: Arc<RegistrationAuthority>,
) {
    match path_tokio_runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(xmpp_worker(cfg, command_rx, output, shutdown, authority)),
        Err(_error) => tracing::warn!(target: LOG_TARGET, "failed to create xmpp runtime"),
    }
}

async fn xmpp_worker(
    cfg: RuntimeConfig,
    command_rx: mpsc::Receiver<XmppCommand>,
    output: Output,
    shutdown: Arc<ShutdownSignal>,
    authority: Arc<RegistrationAuthority>,
) {
    if let Err(error) = path_tokio_xmpp_rustls_crypto::ring::default_provider().install_default() {
        tracing::debug!(target: LOG_TARGET, ?error, "rustls provider was already installed or unavailable");
    }
    let resource = generated_resource(&cfg);
    let login_jid = match Jid::new(&format!("{}/{resource}", cfg.account_jid)) {
        Ok(jid) => jid,
        Err(_error) => {
            tracing::warn!(target: LOG_TARGET, "failed to build xmpp resource jid");
            return;
        }
    };
    let mut client = Client::new(login_jid, cfg.password.expose_secret().to_owned());
    let mut command_rx = std_to_tokio(command_rx);
    let mut worker = WorkerState::new(cfg, output, Arc::clone(&shutdown), authority);
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
                    tokio_xmpp::Event::Disconnected(_error) => {
                        tracing::warn!(target: LOG_TARGET, "xmpp disconnected");
                        worker.handle_disconnected();
                    }
                    tokio_xmpp::Event::Stanza(stanza) => {
                        if worker.handle_stanza(stanza) == WorkerControl::Stop {
                            worker
                                .leave_all_with_timeout(&mut client, WORKER_SHUTDOWN_CLEANUP_TIMEOUT)
                                .await;
                            return;
                        }
                    }
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
) -> path_tokio_sync::mpsc::UnboundedReceiver<T> {
    let (tx, tokio_rx) = path_tokio_sync::mpsc::unbounded_channel();
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

/// Whether the worker may select another stanza or command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkerControl {
    Continue,
    Stop,
}

struct WorkerState {
    /// Runtime config.
    cfg: RuntimeConfig,
    /// Output channel toward the harness.
    output: Output,
    /// Shared shutdown signal used to cancel long best-effort operations.
    shutdown: Arc<ShutdownSignal>,
    /// Generation-bound authority checked at inbound publication.
    authority: Arc<RegistrationAuthority>,
    /// Server-returned bound JID.
    bound_jid: Option<Jid>,
    /// Registered conversations.
    conversations: HashMap<AgentId, Conversation>,
    /// Exact registration generation owning each worker conversation.
    registration_leases: HashMap<AgentId, RegistrationLease>,
    /// MUC joins that have been sent but are not yet routable conversations.
    pending_muc_joins: HashMap<AgentId, MucOccupant>,
    /// MUC room to agent mapping.
    room_to_agent: HashMap<BareJid, AgentId>,
    /// Bounded room-scoped MUC occupant authentication state.
    muc_presence_cache: MucPresenceCache,
    /// Process-unique entropy for locally generated inbound message identities.
    message_id_nonce: [u8; 16],
    /// Monotonic ordinal for inbound stanzas that omit a stanza id.
    next_local_message_id: u64,
    /// Deterministic test barrier immediately before the final authority check.
    #[cfg(test)]
    before_inbound_publication: Option<Box<dyn Fn() + Send + Sync>>,
}

impl WorkerState {
    /// Create a worker state.
    fn new(
        cfg: RuntimeConfig,
        output: impl Into<Output>,
        shutdown: Arc<ShutdownSignal>,
        authority: Arc<RegistrationAuthority>,
    ) -> Self {
        let mut message_id_nonce = [0_u8; 16];
        rand::thread_rng().fill_bytes(&mut message_id_nonce);
        Self {
            cfg,
            output: output.into(),
            shutdown,
            authority,
            bound_jid: None,
            conversations: HashMap::new(),
            registration_leases: HashMap::new(),
            pending_muc_joins: HashMap::new(),
            room_to_agent: HashMap::new(),
            muc_presence_cache: MucPresenceCache::default(),
            message_id_nonce,
            next_local_message_id: 1,
            #[cfg(test)]
            before_inbound_publication: None,
        }
    }

    /// Process one command from tool handlers.
    async fn handle_command(&mut self, command: XmppCommand, client: &mut Client) {
        match command {
            XmppCommand::Register {
                agent_id,
                lease,
                room_localpart,
                response,
            } => {
                self.registration_leases.insert(agent_id.clone(), lease);
                let result = match tokio::time::timeout(
                    REGISTER_TIMEOUT,
                    self.register_agent(agent_id.clone(), room_localpart, client),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => {
                        self.unregister_agent(&agent_id, lease, client).await;
                        Err("timed out registering xmpp conversation".to_owned())
                    }
                };
                if self
                    .finish_register_response(&agent_id, lease, result, response, client)
                    .await
                {
                    self.send_post_register_notice(&agent_id, client).await;
                }
            }
            XmppCommand::Unregister { agent_id, lease } => {
                self.unregister_agent(&agent_id, lease, client).await;
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
        lease: RegistrationLease,
        result: Result<String, String>,
        response: mpsc::Sender<Result<String, String>>,
        client: &mut Client,
    ) -> bool {
        let registered = result.is_ok();
        if response.send(result).is_err() && registered {
            self.unregister_agent(agent_id, lease, client).await;
            return false;
        }
        if !registered {
            self.registration_leases.remove(agent_id);
        }
        registered
    }

    /// Register one agent conversation.
    async fn register_agent(
        &mut self,
        agent_id: AgentId,
        room_localpart: Option<String>,
        client: &mut Client,
    ) -> Result<String, String> {
        if let Some(conversation) = self.conversations.get(&agent_id) {
            return Ok(conversation.address());
        }
        let conversation = match self.cfg.routing_mode {
            RoutingMode::Muc => {
                self.ensure_online(client).await?;
                let room_localpart = room_localpart.ok_or_else(|| {
                    "xmpp MUC registration is missing a rendered room id".to_owned()
                })?;
                let room = self.muc_room_for(&room_localpart)?;
                self.ensure_muc_room_available(&room, &agent_id)?;
                let nick = format!("{}-{}", self.cfg.resource_prefix, short_random_hex());
                let occupant = MucOccupant::new(room.clone(), nick);
                self.pending_muc_joins
                    .insert(agent_id.clone(), occupant.clone());
                self.muc_presence_cache.begin_join(&room);
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

    /// Send the best-effort invitation and fallback notice after registration
    /// has already succeeded for the tool caller.
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
            Err(_error) => {
                tracing::warn!(target: LOG_TARGET, "failed to send xmpp muc invite; sending direct diagnostic notice");
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
        if let Err(_error) = self
            .until_shutdown(send_chat(
                client,
                self.cfg.default_recipient.clone(),
                &notice,
            ))
            .await
        {
            tracing::warn!(target: LOG_TARGET, "failed to send xmpp muc fallback notice after join");
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

    /// Build a MUC room JID from a rendered localpart.
    fn muc_room_for(&self, room_localpart: &str) -> Result<BareJid, String> {
        let service = self
            .cfg
            .muc
            .service
            .clone()
            .ok_or_else(|| "xmpp muc.service is not configured".to_owned())?;
        Jid::new(&format!("{room_localpart}@{}", service.domain()))
            .map_err(|e| format!("failed to build muc room jid: {e}"))
            .map(|jid| jid.to_bare())
    }

    /// Fail closed if a rendered MUC room is already owned by another agent.
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
                        if self.shutdown.is_requested() {
                            return Err("xmpp worker stopped during online setup".to_owned());
                        }
                        return Ok(());
                    }
                    tokio_xmpp::Event::Disconnected(_error) => {
                        tracing::warn!(target: LOG_TARGET, "xmpp disconnected while waiting for online state");
                    }
                    tokio_xmpp::Event::Stanza(stanza) => {
                        self.handle_nested_stanza(stanza, "online state")?;
                    }
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
        self.muc_presence_cache.clear_connection();
    }

    /// Revoke an exact worker route before best-effort remote cleanup.
    async fn unregister_agent(
        &mut self,
        agent_id: &AgentId,
        lease: RegistrationLease,
        client: &mut Client,
    ) {
        let Some((pending, conversation)) = self.retire_registration(agent_id, lease) else {
            return;
        };
        if let Some(occupant) = pending {
            self.leave_muc_occupant(&occupant, client).await;
        }
        if let Some(conversation) = conversation {
            self.leave_conversation(&conversation, client).await;
        }
    }

    /// Retire an exact worker generation without touching a newer registration.
    fn retire_registration(
        &mut self,
        agent_id: &AgentId,
        lease: RegistrationLease,
    ) -> Option<(Option<MucOccupant>, Option<Conversation>)> {
        if self.registration_leases.get(agent_id) != Some(&lease) {
            return None;
        }
        self.registration_leases.remove(agent_id);
        let pending = self.pending_muc_joins.remove(agent_id);
        if let Some(occupant) = pending.as_ref() {
            self.muc_presence_cache.purge_room(&occupant.room);
        }
        let conversation = self.remove_conversation(agent_id);
        Some((pending, conversation))
    }

    /// Remove one registered conversation and its room mapping.
    fn remove_conversation(&mut self, agent_id: &AgentId) -> Option<Conversation> {
        let conversation = self.conversations.remove(agent_id);
        if let Some(occupant) = self.pending_muc_joins.remove(agent_id) {
            self.muc_presence_cache.purge_room(&occupant.room);
        }
        if let Some(Conversation::Muc { room, .. }) = conversation.as_ref() {
            self.muc_presence_cache.purge_room(room);
        }
        self.room_to_agent.retain(|_, mapped| mapped != agent_id);
        conversation
    }

    /// Leave all registered MUC conversations before worker shutdown, bounded
    /// by one overall cleanup budget.
    async fn leave_all_with_timeout(&mut self, client: &mut Client, timeout: Duration) {
        let deadline = path_tokio_time::Instant::now() + timeout;
        self.muc_presence_cache.clear_connection();
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
        self.registration_leases.clear();
    }

    /// Send leave presence for a MUC conversation. Direct conversations require
    /// no per-conversation unavailable stanza.
    async fn leave_conversation(&self, conversation: &Conversation, client: &mut Client) {
        if let Conversation::Muc { room, nick } = conversation
            && let Err(_error) = leave_room(client, room, nick).await
        {
            tracing::warn!(target: LOG_TARGET, "failed to leave xmpp muc room");
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
            && let Err(_error) = leave_room_until(client, room, nick, deadline).await
        {
            tracing::warn!(target: LOG_TARGET, "failed to leave xmpp muc room during shutdown");
        }
    }

    /// Leave a pending MUC join and remove its non-routable registration state.
    async fn leave_pending_muc_join(&mut self, agent_id: &AgentId, client: &mut Client) {
        if let Some(occupant) = self.pending_muc_joins.get(agent_id).cloned() {
            self.muc_presence_cache.purge_room(&occupant.room);
            self.leave_muc_occupant(&occupant, client).await;
            self.pending_muc_joins.remove(agent_id);
        }
    }

    /// Send unavailable presence for one MUC room/nick pair.
    async fn leave_muc_occupant(&self, occupant: &MucOccupant, client: &mut Client) {
        if let Err(_error) = leave_room(client, &occupant.room, &occupant.nick).await {
            tracing::warn!(target: LOG_TARGET, "failed to leave pending xmpp muc room");
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
        if let Err(_error) =
            leave_room_until(client, &occupant.room, &occupant.nick, deadline).await
        {
            tracing::warn!(target: LOG_TARGET, "failed to leave pending xmpp muc room during shutdown");
        }
    }

    /// Refresh connection-dependent state after the XMPP stream comes online.
    async fn handle_online(&mut self, bound_jid: Jid, client: &mut Client) {
        self.refresh_online_state(bound_jid, client).await;
        if self.rejoin_all(client).await == WorkerControl::Stop {
            self.shutdown.request();
        }
    }

    /// Refresh online state without recursively rejoining rooms.
    async fn refresh_online_state(&mut self, bound_jid: Jid, client: &mut Client) {
        let direct_updates = self.apply_online_state(bound_jid);
        if let Err(_error) = send_presence(client, Presence::available().with_priority(-1)).await {
            tracing::warn!(target: LOG_TARGET, "failed to send xmpp available presence");
        }
        self.notify_direct_reconnects(direct_updates, client).await;
    }

    /// Apply state changes for a newly online stream and return direct-resource
    /// registrations whose externally visible address changed.
    fn apply_online_state(&mut self, bound_jid: Jid) -> Vec<(AgentId, Jid)> {
        self.bound_jid = Some(bound_jid.clone());
        self.muc_presence_cache.clear_connection();
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
            if let Err(_error) =
                send_chat(client, self.cfg.default_recipient.clone(), &notice).await
            {
                tracing::warn!(target: LOG_TARGET, "failed to send xmpp direct-resource reconnect notice");
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
        let conversation = self.conversations.get(agent_id).ok_or_else(|| {
            "The XMPP send tool requires the registration tool with enabled=true first".to_owned()
        })?;
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
        tracing::debug!(
            target: LOG_TARGET,
            room = %occupant.room,
            nick = %occupant.nick,
            created = join.created,
            statuses = ?join.statuses,
            "joined xmpp muc room"
        );
        if join.created {
            tracing::debug!(target: LOG_TARGET, room = %occupant.room, "new xmpp muc room created; submitting instant-room owner config");
            submit_instant_room_config(client, &occupant.room).await?;
            tracing::debug!(target: LOG_TARGET, room = %occupant.room, "submitted xmpp muc instant-room owner config");
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
                    tokio_xmpp::Event::Disconnected(_error) => {
                        self.handle_disconnected();
                        tracing::warn!(target: LOG_TARGET, "xmpp disconnected while waiting for muc join confirmation");
                    }
                    tokio_xmpp::Event::Stanza(Stanza::Presence(presence))
                        if muc_presence_from(&presence, &occupant) =>
                    {
                        return self.handle_muc_self_presence(room, presence);
                    }
                    tokio_xmpp::Event::Stanza(stanza) => {
                        self.handle_nested_stanza(stanza, "muc join")?;
                    }
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

    /// Validate correlated self-presence and reject an incomplete initial
    /// roster.
    fn handle_muc_self_presence(
        &mut self,
        room: &BareJid,
        presence: Presence,
    ) -> Result<MucJoin, String> {
        let join = MucJoin::from_self_presence(&presence)?;
        self.handle_presence(presence);
        if self.is_pending_muc_room(room) && self.muc_presence_cache.is_quarantined(room) {
            return Err(format!(
                "xmpp MUC occupant roster exceeds cache limits ({MAX_MUC_OCCUPANTS_PER_ROOM} per room, {MAX_MUC_OCCUPANTS_TOTAL} total); registration was not installed"
            ));
        }
        Ok(join)
    }

    /// Rejoin all known MUC rooms after an online/reconnect event.
    async fn rejoin_all(&mut self, client: &mut Client) -> WorkerControl {
        for (room, nick) in self.muc_rooms_to_rejoin() {
            let occupant = MucOccupant::new(room, nick);
            self.muc_presence_cache.begin_join(&occupant.room);
            if let Err(_error) = join_room(client, &occupant.room, &occupant.nick).await {
                tracing::warn!(target: LOG_TARGET, "failed to rejoin xmpp muc room");
                continue;
            }
            if let Err(_error) = self.setup_joined_muc_room(client, &occupant).await {
                tracing::warn!(target: LOG_TARGET, "failed to confirm/setup rejoined xmpp muc room");
                if self.shutdown.is_requested() {
                    return WorkerControl::Stop;
                }
            }
        }
        WorkerControl::Continue
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

    /// Process an inbound stanza, returning false when the worker must stop
    /// before selecting any later stanza or command.
    fn handle_stanza(&mut self, stanza: Stanza) -> WorkerControl {
        if self.shutdown.is_requested() {
            return WorkerControl::Stop;
        }
        match stanza {
            Stanza::Message(message) => self.handle_message(message),
            Stanza::Presence(presence) => self.handle_presence(presence),
            Stanza::Iq(iq) => self.handle_iq(iq),
        }
        if self.shutdown.is_requested() {
            WorkerControl::Stop
        } else {
            WorkerControl::Continue
        }
    }

    /// Propagate nested readiness/MUC reader shutdown to the owning operation.
    fn handle_nested_stanza(&mut self, stanza: Stanza, context: &str) -> Result<(), String> {
        match self.handle_stanza(stanza) {
            WorkerControl::Continue => Ok(()),
            WorkerControl::Stop => Err(format!("xmpp worker stopped while waiting for {context}")),
        }
    }

    /// Process inbound IQ stanzas not already claimed by an explicit IQ
    /// response token.
    fn handle_iq(&self, iq: Iq) {
        if let Iq::Error { .. } = iq {
            tracing::warn!(target: LOG_TARGET, "received xmpp iq error");
        }
    }

    /// Process inbound presence for MUC real-JID allowlist enforcement.
    fn handle_presence(&mut self, presence: Presence) {
        let Some(from) = presence.from.clone() else {
            return;
        };
        let Ok(occupant) = from.clone().try_into_full() else {
            return;
        };
        let room = occupant.to_bare();
        if !self.is_tracked_muc_room(&room) {
            return;
        }
        if matches!(
            presence.type_,
            PresenceType::Unavailable | PresenceType::Error
        ) {
            self.muc_presence_cache.remove(&occupant);
            if presence.type_ == PresenceType::Error {
                let _error = presence
                    .payloads
                    .iter()
                    .find_map(|payload| StanzaError::try_from(payload.clone()).ok());
                tracing::warn!(target: LOG_TARGET, "received xmpp presence error");
            }
            return;
        }
        if presence.type_ != PresenceType::None {
            return;
        }
        for payload in presence.payloads {
            let Ok(muc_user) = MucUser::try_from(payload) else {
                continue;
            };
            let Some(real_jid) = muc_user.items.iter().find_map(|item| item.jid.clone()) else {
                continue;
            };
            if self.muc_presence_cache.admit(occupant.clone(), real_jid) == Admission::Quarantined
                && self.room_to_agent.contains_key(&room)
                && self.muc_presence_cache.take_warning(&room)
            {
                tracing::warn!(
                    target: LOG_TARGET,
                    per_room_limit = MAX_MUC_OCCUPANTS_PER_ROOM,
                    worker_limit = MAX_MUC_OCCUPANTS_TOTAL,
                    "xmpp MUC presence cache limit reached; quarantining room until a fresh join"
                );
            }
        }
    }

    /// Return whether one room is active or is the exact room currently
    /// joining.
    fn is_tracked_muc_room(&self, room: &BareJid) -> bool {
        self.room_to_agent.contains_key(room) || self.is_pending_muc_room(room)
    }

    /// Return whether one room is an exact pending MUC join.
    fn is_pending_muc_room(&self, room: &BareJid) -> bool {
        self.pending_muc_joins
            .values()
            .any(|occupant| &occupant.room == room)
    }

    /// Process inbound message stanzas.
    fn handle_message(&mut self, message: Message) {
        if message.type_ == MessageType::Error {
            let _error = message
                .payloads
                .iter()
                .find_map(|payload| StanzaError::try_from(payload.clone()).ok());
            tracing::warn!(target: LOG_TARGET, "received xmpp message error");
            return;
        }
        // XEP-0203 delayed delivery marks backlog/history. The MVP is live-only,
        // so delayed messages must not become fresh Tau report submissions.
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
        if self.muc_presence_cache.is_quarantined(&room) {
            return;
        }
        let Some(lease) = self.registration_leases.get(&agent_id).copied() else {
            return;
        };
        if self.is_own_muc_message(&agent_id, &from) {
            return;
        }
        let Ok(occupant) = from.clone().try_into_full() else {
            return;
        };
        let real = self.muc_presence_cache.get(&occupant).cloned();
        if real.is_none() && !self.cfg.muc.trust_muc_membership {
            tracing::warn!(
                target: LOG_TARGET,
                expose_real_jids = self.cfg.muc.expose_real_jids,
                "dropping muc message without real jid proof"
            );
            return;
        }
        if let Some(real_jid) = real.as_ref()
            && !self.cfg.is_allowed(real_jid)
        {
            tracing::warn!(target: LOG_TARGET, "dropping muc message from non-allowlisted real jid");
            return;
        }
        let sender_id = real
            .as_ref()
            .map(|jid| jid.to_bare().to_string())
            .unwrap_or_else(|| from.to_string());
        let conversation_id = room.to_string();
        let message_id = self.inbound_message_id(&message, &sender_id, &conversation_id);
        if self
            .route(
                agent_id,
                lease,
                message_id,
                MessageParty {
                    stable_id: xmpp_sender_ref(&sender_id),
                    display_name: from
                        .resource()
                        .and_then(|resource| bounded_xmpp_display_name(resource.as_str())),
                    sender_auth: Some(if real.is_some() {
                        MessageSenderAuth::VerifiedAllowlisted
                    } else {
                        MessageSenderAuth::TrustedMembership
                    }),
                },
                conversation_id,
                body,
            )
            .is_err()
        {
            self.shutdown.request();
        }
    }

    /// Process inbound direct chat fallback.
    fn handle_direct(&mut self, message: Message, body: String) {
        let Some(from) = message.from.clone() else {
            return;
        };
        if !self.cfg.is_allowed(&from) {
            tracing::warn!(target: LOG_TARGET, "dropping direct xmpp message from non-allowlisted jid");
            return;
        }
        let Some(to) = message.to.as_ref() else {
            return;
        };
        let Some(bound) = self.bound_jid.as_ref() else {
            return;
        };
        if to != bound {
            tracing::warn!(target: LOG_TARGET, "dropping direct xmpp message not addressed to the current bound full jid");
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
            tracing::warn!(target: LOG_TARGET, "dropping direct xmpp message with no registered direct-resource agent; in MUC mode, send messages in the agent room instead of replying to direct notices");
            return;
        }
        let Some(lease) = self.registration_leases.get(&agents[0]).copied() else {
            return;
        };
        let sender_id = from.to_bare().to_string();
        let conversation_id = sender_id.clone();
        let message_id = self.inbound_message_id(&message, &sender_id, &conversation_id);
        if self
            .route(
                agents[0].clone(),
                lease,
                message_id,
                MessageParty {
                    stable_id: xmpp_sender_ref(&sender_id),
                    display_name: from
                        .resource()
                        .and_then(|resource| bounded_xmpp_display_name(resource.as_str())),
                    sender_auth: Some(MessageSenderAuth::VerifiedAllowlisted),
                },
                conversation_id,
                body,
            )
            .is_err()
        {
            self.shutdown.request();
        }
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

    /// Submit an accepted XMPP message as a transient delivered report.
    fn route(
        &self,
        agent_id: AgentId,
        lease: RegistrationLease,
        message_id: MessageFactId,
        sender: MessageParty,
        conversation_id: String,
        text: String,
    ) -> ClientResult<()> {
        #[cfg(test)]
        if let Some(hook) = self.before_inbound_publication.as_ref() {
            hook();
        }
        self.authority
            .publish_if_active(&agent_id, lease, || {
                self.output
                    .emit_message_report(Event::MessageDeliveredReported(MessageDelivered::new(
                        RawMessagePublisherId::new(self.cfg.instance_name.as_str()),
                        MessageAgentTarget::new(agent_id.as_ref()),
                        message_id,
                        sender,
                        Some(MessageConversation {
                            stable_id: conversation_id,
                            display_name: None,
                            alias: None,
                        }),
                        text,
                    )))
            })
            .unwrap_or(Ok(()))
    }

    /// Build a bounded publisher-scoped identity from a stanza id or a
    /// process-unique local nonce and ordinal when the stanza omits one.
    fn inbound_message_id(
        &mut self,
        message: &Message,
        sender_id: &str,
        conversation_id: &str,
    ) -> MessageFactId {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"tau-ext-xmpp/message.delivered/v1\0");
        hasher.update(sender_id.as_bytes());
        hasher.update(b"\0");
        hasher.update(conversation_id.as_bytes());
        if let Some(stanza_id) = message.id.as_ref() {
            hasher.update(b"\0stanza\0");
            hasher.update(stanza_id.0.as_bytes());
        } else {
            hasher.update(b"\0local\0");
            hasher.update(&self.message_id_nonce);
            hasher.update(&self.next_local_message_id.to_be_bytes());
            self.next_local_message_id = self.next_local_message_id.wrapping_add(1);
        }
        MessageFactId::new(format!("xmpp-delivered:{}", hasher.finalize().to_hex()))
    }
}

/// Derive a bounded publisher-unique send identity from the harness-unique tool
/// call and extension-authoritative conversation.
fn generated_xmpp_send_message_id(call_id: &str, conversation: &str) -> MessageFactId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"tau-ext-xmpp/message.sent/v1\0");
    hasher.update(call_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(conversation.as_bytes());
    MessageFactId::new(format!("xmpp-send:{}", hasher.finalize().to_hex()))
}

/// Derive an opaque canonical sender reference as required by
/// `SPEC-external-message-reports-and-facts`.
fn xmpp_sender_ref(sender_id: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"tau-ext-xmpp/sender-ref/v1\0");
    hasher.update(sender_id.as_bytes());
    format!("xmpp-sender:{}", hasher.finalize().to_hex())
}

/// Bound an XMPP nickname/resource to the universal message-fact display limit.
fn bounded_xmpp_display_name(value: &str) -> Option<String> {
    let mut out = String::new();
    for ch in value.trim().chars().take(80) {
        if out.len() + ch.len_utf8() > 256 {
            break;
        }
        out.push(ch);
    }
    (!out.is_empty()).then_some(out)
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
            tracing::warn!(target: LOG_TARGET, "xmpp muc instant-room owner config rejected");
            Err(format!(
                "xmpp MUC instant-room owner config rejected by server: {:?} {:?}",
                error.type_, error.defined_condition
            ))
        }
        Ok(Err(error)) => {
            tracing::warn!(target: LOG_TARGET, "failed to send xmpp muc instant-room owner config iq");
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
    let form = path_xmpp_parsers_minidom::Element::builder("x", ns::DATA_FORMS)
        .attr(
            path_xmpp_parsers_minidom::rxml::xml_ncname!("type").into(),
            "submit",
        )
        .build();
    path_xmpp_parsers_minidom::Element::builder("query", MUC_OWNER_NS)
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
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    let mut runtime = match tau_client::TauExtensionRunner::new(XmppExtension)
        .start_manual_loop_with_state(reader, writer, move |handle| XmppRuntime {
            ext: Extension::new(bridge, handle),
        }) {
        Ok(runtime) => runtime,
        Err(ClientError::InitialConfigureRejected) => return Ok(()),
        Err(error) => return Err(Box::new(error)),
    };
    runtime.state().ext.output.install_waker(runtime.waker());
    let loop_result = run_xmpp_loop(&mut runtime);
    runtime.state().ext.revoke_all();
    runtime.state().ext.shutdown.request();
    match loop_result {
        Ok(true) => {
            let _ = runtime.finish_detached();
            Ok(())
        }
        Ok(false) => runtime
            .finish()
            .map(|_| ())
            .map_err(|error| Box::new(error) as Box<dyn Error>),
        Err(error) => {
            let _ = runtime.finish();
            Err(Box::new(error))
        }
    }
}

/// Drive harness input and retire the connection after worker output failure.
fn run_xmpp_loop(
    runtime: &mut tau_client::ManualExtensionRuntime<XmppRuntime>,
) -> ClientResult<bool> {
    loop {
        runtime.state().ext.output.check_mandatory_output()?;
        match runtime.try_recv()? {
            ManualRuntimePoll::Message(message) => match runtime.dispatch_one(message)? {
                tau_client::DispatchOutcome::Continue => {}
                tau_client::DispatchOutcome::StopRequested => break Ok(false),
                tau_client::DispatchOutcome::Disconnect(_) => break Ok(true),
            },
            ManualRuntimePoll::InputClosed => break Ok(false),
            ManualRuntimePoll::Empty => runtime.wait_for_wake(),
        }
    }
}

struct XmppExtension;

impl TauExtension for XmppExtension {
    type State = XmppRuntime;

    fn name(&self) -> &'static str {
        "tau-ext-xmpp"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder
            .message_bridge()
            .configure_raw(handle_configure)
            .scoped_tool(
                tau_proto::ToolName::new(REGISTER_TOOL_NAME),
                |scope| {
                    let mut tool = register_tool_spec();
                    let send = scope.wire_tool(SEND_TOOL_NAME)?;
                    tool.description = Some(format!(
                        "Register or unregister the current agent for XMPP messages. Incoming messages are accepted only from configured allowed_jids. Use {send} to reply to XMPP-originated message facts."
                    ));
                    Ok(tau_proto::ToolRegistrationDeclared {
                        tool,
                        tool_group: Some(xmpp_tool_group()),
                        prompt_fragment: None,
                    })
                },
                handle_tool_invocation,
            )
            .scoped_tool(
                tau_proto::ToolName::new(SEND_TOOL_NAME),
                |scope| {
                    let mut tool = send_tool_spec();
                    let register = scope.wire_tool(REGISTER_TOOL_NAME)?;
                    tool.description = Some(format!(
                        "Send a text reply to this agent's registered XMPP room or direct conversation. There is no destination argument; use {register} first. Replies to room-message prompts are visible to room occupants."
                    ));
                    Ok(tau_proto::ToolRegistrationDeclared {
                        tool,
                        tool_group: Some(xmpp_tool_group()),
                        prompt_fragment: None,
                    })
                },
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
            .on_raw_restore(
                tau_proto::EventSelector::Exact(tau_proto::EventName::AGENT_STARTED),
                handle_template_metadata,
            )
            .on_raw_live(
                tau_proto::EventSelector::Exact(tau_proto::EventName::AGENT_STARTED),
                handle_template_metadata,
            )
            .on_raw_restore(
                tau_proto::EventSelector::Exact(tau_proto::EventName::HARNESS_ROLES_AVAILABLE),
                handle_template_metadata,
            )
            .on_raw_live(
                tau_proto::EventSelector::Exact(tau_proto::EventName::HARNESS_ROLES_AVAILABLE),
                handle_template_metadata,
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
    let instance_name = cx.instance_name().clone();
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
    log_xmpp_configured();
    Ok(())
}

/// Emit the accepted-configuration baseline without routing identities.
fn log_xmpp_configured() {
    tracing::info!(target: LOG_TARGET, "xmpp configured");
}

fn handle_tool_invocation(cx: tau_client::ToolContext<'_, XmppRuntime>) -> ClientResult<()> {
    let local = cx.local_tool_name().clone();
    cx.state
        .ext
        .dispatch_scoped_tool(&local, cx.invoke().clone())
}

fn handle_session_started(cx: tau_client::RawEventContext<'_, XmppRuntime>) -> ClientResult<()> {
    if let Event::SessionStarted(started) = cx.event() {
        let mut state = cx.state.ext.state.lock().unwrap_or_else(|e| e.into_inner());
        match state.current_session_id.as_ref() {
            Some(current) if current != &started.session_id => {
                return Err(ClientError::handler(format!(
                    "immutable session mismatch: expected `{current}`, received `{}`",
                    started.session_id
                )));
            }
            Some(_) => {}
            None => state.current_session_id = Some(started.session_id.clone()),
        }
    }
    Ok(())
}

fn handle_template_metadata(cx: tau_client::RawEventContext<'_, XmppRuntime>) -> ClientResult<()> {
    let mut state = cx.state.ext.state.lock().unwrap_or_else(|e| e.into_inner());
    match cx.event() {
        Event::AgentStarted(started) => {
            state
                .agent_roles
                .insert(started.agent_id.clone(), started.role.clone());
            if started.ephemeral {
                state.ephemeral_agent_roles.insert(started.agent_id.clone());
            } else {
                state.ephemeral_agent_roles.remove(&started.agent_id);
            }
        }
        Event::HarnessRolesAvailable(available) => {
            let mut role_groups = available
                .roles
                .iter()
                .map(|role| (role.name.clone(), role.name.clone()))
                .collect::<HashMap<_, _>>();
            for group in &available.groups {
                for role in &group.roles {
                    role_groups.insert(role.clone(), group.name.clone());
                }
            }
            state.role_groups = role_groups;
        }
        _ => {}
    }
    Ok(())
}

fn handle_live_event(cx: tau_client::RawEventContext<'_, XmppRuntime>) -> ClientResult<()> {
    match cx.event() {
        Event::SessionAgentUnloaded(unloaded) => {
            unload_agent(&cx.state.ext, unloaded.agent_id.clone());
        }
        Event::SessionShutdown(shutdown) => {
            let bound = cx
                .state
                .ext
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .current_session_id
                .clone();
            if bound.as_ref() != Some(&shutdown.session_id) {
                return Err(ClientError::handler(
                    "session shutdown does not match immutable binding",
                ));
            }
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
    ext.revoke_agent(&agent_id);
    let mut state = ext.state.lock().unwrap_or_else(|e| e.into_inner());
    if state.ephemeral_agent_roles.remove(&agent_id) {
        state.agent_roles.remove(&agent_id);
    }
}

fn shutdown_session(ext: &Extension, session_id: SessionId) {
    {
        let mut state = ext.state.lock().unwrap_or_else(|e| e.into_inner());
        for agent_id in state.ephemeral_agent_roles.drain().collect::<Vec<_>>() {
            state.agent_roles.remove(&agent_id);
        }
        // ast-grep-ignore: debug-assert-expression-must-not-mutate
        debug_assert!(
            state
                .current_session_id
                .as_ref()
                .is_none_or(|bound| bound == &session_id),
            "shutdown must not target another session"
        );
    }
    ext.revoke_all();
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
        description: Some("Register or unregister the current agent for XMPP messages. Incoming messages are accepted only from configured allowed_jids. Use xmpp_send to reply to XMPP-originated message facts.".to_owned()),
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
    let instance = clean_token_or(cfg.instance_name.as_str(), "session");
    format!(
        "{}-{}-{}-{}",
        cfg.resource_prefix,
        instance,
        std::process::id(),
        short_random_hex()
    )
}

fn muc_room_disambiguator(agent_id: &AgentId) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"tau-ext-xmpp muc room v2\0agent\0");
    hasher.update(agent_id.as_ref().as_bytes());
    let hash = hasher.finalize();
    base32_token(&hash.as_bytes()[..MUC_ROOM_DISAMBIGUATOR_BYTES])
}

fn base32_token(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";
    let mut out = String::new();
    let mut buffer = 0u16;
    let mut bits = 0u8;
    for byte in bytes {
        buffer = (buffer << 8) | u16::from(*byte);
        bits += 8;
        while 5 <= bits {
            bits -= 5;
            let index = usize::from((buffer >> bits) & 0b1_1111);
            out.push(char::from(ALPHABET[index]));
        }
    }
    if 0 < bits {
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
