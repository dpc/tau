mod proactive_direct_message;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

pub(crate) use proactive_direct_message::DirectRoute;
use proactive_direct_message::ProactiveDirectMessageConfig;
use tau_proto::SecretValue;

use crate::{DEFAULT_MAX_MESSAGE_BYTES, MAX_MESSAGE_BYTES};

/// Raw Zulip extension configuration supplied by the harness.
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ExtConfig {
    /// Secret containing the bot email used for HTTP Basic authentication.
    pub(crate) bot_email_secret: Option<String>,
    /// Secret containing the bot API key used for HTTP Basic authentication.
    pub(crate) api_key_secret: Option<String>,
    /// Stable secret key for opaque publisher-domain identifiers.
    pub(crate) identity_key_secret: Option<String>,
    /// Zulip organization URL, or an API base ending in `/api/v1` for tests.
    pub(crate) site: Option<String>,
    /// Zulip user IDs admitted as external senders.
    pub(crate) allowed_user_ids: Vec<u64>,
    /// Optional stable presentation aliases for admitted senders.
    pub(crate) sender_aliases: Vec<SenderAliasConfig>,
    /// Exact configured stream/topic routes.
    pub(crate) conversations: Vec<ConversationConfig>,
    /// Explicit proactive direct-message destinations.
    pub(crate) proactive_direct_messages: Vec<ProactiveDirectMessageConfig>,
    /// Whether one-to-one and group direct messages may be received.
    pub(crate) direct_messages: Option<DirectMessageConfig>,
    /// Maximum UTF-8 bytes accepted for inbound and outbound text.
    pub(crate) max_message_bytes: Option<usize>,
    /// Recover newly created messages missed while the extension was offline.
    pub(crate) offline_message_catch_up: bool,
}

/// Configured presentation alias for one Zulip user.
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SenderAliasConfig {
    /// Exact numeric Zulip user ID.
    pub(crate) user_id: u64,
    /// Bounded operator-chosen presentation alias.
    pub(crate) alias: String,
}

/// Configured Zulip stream route resolved to a private native ID at
/// registration.
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConversationConfig {
    /// Exact Zulip channel name resolved before queue registration.
    pub(crate) name: String,
    /// Optional exact topic; omission covers every topic in the stream.
    pub(crate) topic: Option<String>,
    /// Optional ingress mode for this route.
    pub(crate) receive: Option<ReceiveMode>,
    /// Whether agents may proactively send through this alias.
    #[serde(default)]
    pub(crate) proactive_send: bool,
    /// Whether this destination lets an agent choose a topic in its fixed
    /// stream.
    #[serde(default)]
    pub(crate) agent_chosen_topic: bool,
    /// Optional trusted operator description returned by discovery.
    pub(crate) description: Option<String>,
}

/// Direct-message ingress configuration.
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DirectMessageConfig {
    /// Direct messages are always admitted as all-message traffic.
    pub(crate) receive: ReceiveMode,
}

/// Ingress mode for a configured conversation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReceiveMode {
    /// Admit only messages carrying a direct personal mention of this bot.
    MentionsOnly,
    /// Admit every allowlisted message on the route.
    AllMessages,
}

/// Configured channel route before registration resolves its native ID.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConfiguredStreamRoute {
    /// Exact configured Zulip channel name.
    pub(crate) name: String,
    /// Optional exact topic.
    pub(crate) topic: Option<String>,
    /// Optional receive authority.
    pub(crate) receive: Option<ReceiveMode>,
    /// Proactive destination authority derived from operator configuration.
    pub(crate) proactive: ProactiveRoute,
    /// Trusted bounded description.
    pub(crate) description: Option<String>,
}

impl ConfiguredStreamRoute {
    /// Bind this configured route to one resolved native channel ID.
    pub(crate) fn resolve(&self, stream_id: u64) -> StreamRoute {
        StreamRoute {
            name: self.name.clone(),
            stream_id,
            topic: self.topic.clone(),
            receive: self.receive,
            proactive: self.proactive.clone(),
            description: self.description.clone(),
        }
    }
}

/// Resolved route retained as extension-private native authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StreamRoute {
    /// Exact configured Zulip channel name.
    pub(crate) name: String,
    /// Native stream ID resolved before installing a live queue.
    pub(crate) stream_id: u64,
    /// Optional exact topic.
    pub(crate) topic: Option<String>,
    /// Optional receive authority.
    pub(crate) receive: Option<ReceiveMode>,
    /// Proactive destination authority derived from the operator configuration.
    pub(crate) proactive: ProactiveRoute,
    /// Trusted bounded description.
    pub(crate) description: Option<String>,
}

/// Validated proactive topic authority for one configured stream destination.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ProactiveRoute {
    /// This route cannot send proactively.
    Disabled,
    /// This destination sends only to its configured exact topic.
    ExactTopic(String),
    /// This destination permits an agent-selected topic in its configured
    /// stream.
    AgentChosenTopic,
}

impl ProactiveRoute {
    /// Return whether the route may be selected for proactive sends.
    pub(crate) fn is_enabled(&self) -> bool {
        !matches!(self, Self::Disabled)
    }

    /// Return whether the route explicitly grants agent-chosen topic authority.
    pub(crate) fn allows_agent_chosen_topic(&self) -> bool {
        matches!(self, Self::AgentChosenTopic)
    }
}

/// Validated runtime configuration including resolved secrets.
#[derive(Clone)]
pub(crate) struct RuntimeConfig {
    /// Bot email secret; never log this value.
    pub(crate) email: String,
    /// Bot API key secret; never log this value.
    pub(crate) api_key: String,
    /// API base ending in `/api/v1`.
    pub(crate) api_base: String,
    /// Exact sender allowlist.
    pub(crate) allowed_user_ids: HashSet<u64>,
    /// Presentation aliases indexed by sender ID.
    pub(crate) sender_aliases: HashMap<u64, String>,
    /// Configured stream routes awaiting native ID resolution.
    pub(crate) configured_routes: Vec<ConfiguredStreamRoute>,
    /// Resolved stream routes installed with the current queue.
    pub(crate) routes: Vec<StreamRoute>,
    /// Configured proactive direct-message routes.
    pub(crate) direct_routes: Vec<DirectRoute>,
    /// Whether direct-message ingress is enabled.
    pub(crate) receive_direct_messages: bool,
    /// Message size ceiling.
    pub(crate) max_message_bytes: usize,
    /// Secret-derived key for non-reversible descriptive identifiers.
    pub(crate) id_key: [u8; 32],
    /// Whether durable newly-created-message catch-up is enabled.
    pub(crate) offline_message_catch_up: bool,
    /// Harness-assigned extension state directory.
    pub(crate) state_dir: Option<PathBuf>,
}

impl ExtConfig {
    /// Resolve secrets and reject ambiguous or unsafe routing policy.
    pub(crate) fn validate(
        self,
        secrets: &BTreeMap<String, SecretValue>,
    ) -> Result<RuntimeConfig, String> {
        let email = secret(secrets, self.bot_email_secret, "bot_email_secret")?;
        let api_key = secret(secrets, self.api_key_secret, "api_key_secret")?;
        let identity_key = secret(secrets, self.identity_key_secret, "identity_key_secret")?;
        if self.allowed_user_ids.is_empty() {
            return Err("zulip config requires non-empty `allowed_user_ids`".to_owned());
        }
        if 64 < self.conversations.len() + self.proactive_direct_messages.len()
            || 64 < self.sender_aliases.len()
        {
            return Err("zulip config exceeds the 64-entry route or alias limit".to_owned());
        }
        let site = self
            .site
            .ok_or_else(|| "zulip config requires `site`".to_owned())?;
        let api_base = normalize_api_base(&site)?;
        let mut id_key_hasher = blake3::Hasher::new();
        id_key_hasher.update(b"tau-ext-zulip/id-key/v1\0");
        id_key_hasher.update(identity_key.as_bytes());
        let id_key = *id_key_hasher.finalize().as_bytes();
        let max_message_bytes = self.max_message_bytes.unwrap_or(DEFAULT_MAX_MESSAGE_BYTES);
        if max_message_bytes == 0 || MAX_MESSAGE_BYTES < max_message_bytes {
            return Err(format!(
                "zulip `max_message_bytes` must be in 1..={MAX_MESSAGE_BYTES}"
            ));
        }
        let mut aliases = HashMap::new();
        let mut sender_alias_values = HashSet::new();
        for entry in self.sender_aliases {
            validate_alias(&entry.alias)?;
            if !self.allowed_user_ids.contains(&entry.user_id)
                || aliases.insert(entry.user_id, entry.alias.clone()).is_some()
                || !sender_alias_values.insert(entry.alias)
            {
                return Err(
                    "zulip sender aliases must be unique and refer to allowlisted users".to_owned(),
                );
            }
        }
        let mut destination_names = HashSet::new();
        let mut configured_routes = Vec::new();
        for route in self.conversations {
            validate_stream_name(&route.name)?;
            validate_topic(route.topic.as_deref())?;
            validate_description(route.description.as_deref())?;
            if route.receive.is_none() && !route.proactive_send {
                return Err(
                    "each Zulip conversation needs receive or proactive_send authority".to_owned(),
                );
            }
            if route.agent_chosen_topic && !route.proactive_send {
                return Err("zulip `agent_chosen_topic` requires `proactive_send: true`".to_owned());
            }
            if route.agent_chosen_topic && route.topic.is_some() {
                return Err(
                    "zulip `agent_chosen_topic` routes must omit the configured `topic`".to_owned(),
                );
            }
            if route.proactive_send && route.topic.is_none() && !route.agent_chosen_topic {
                return Err("proactive Zulip stream routes require an exact `topic` or \
                     `agent_chosen_topic: true`"
                    .to_owned());
            }
            if !destination_names.insert(route.name.clone()) {
                return Err("zulip conversation names must be unique".to_owned());
            }
            let proactive = if route.agent_chosen_topic {
                ProactiveRoute::AgentChosenTopic
            } else if route.proactive_send {
                ProactiveRoute::ExactTopic(
                    route
                        .topic
                        .clone()
                        .expect("validated proactive exact topic"),
                )
            } else {
                ProactiveRoute::Disabled
            };
            configured_routes.push(ConfiguredStreamRoute {
                name: route.name,
                topic: route.topic,
                receive: route.receive,
                proactive,
                description: route.description,
            });
        }
        let mut direct_routes = Vec::new();
        for route in self.proactive_direct_messages {
            direct_routes.push(route.validate(&mut destination_names)?);
        }
        let receive_direct_messages = match self.direct_messages {
            None => false,
            Some(config) if config.receive == ReceiveMode::AllMessages => true,
            Some(_) => {
                return Err("zulip direct_messages receive must be `all_messages`".to_owned());
            }
        };
        Ok(RuntimeConfig {
            email,
            api_key,
            api_base,
            allowed_user_ids: self.allowed_user_ids.into_iter().collect(),
            sender_aliases: aliases,
            configured_routes,
            routes: Vec::new(),
            direct_routes,
            receive_direct_messages,
            max_message_bytes,
            id_key,
            offline_message_catch_up: self.offline_message_catch_up,
            state_dir: None,
        })
    }
}

fn validate_stream_name(value: &str) -> Result<(), String> {
    let valid = !value.trim().is_empty()
        && value.len() <= 256
        && !value.chars().any(tau_proto::requires_visible_escape);
    valid
        .then_some(())
        .ok_or_else(|| "zulip channel names must be visible and at most 256 bytes".to_owned())
}

fn secret(
    secrets: &BTreeMap<String, SecretValue>,
    name: Option<String>,
    field: &str,
) -> Result<String, String> {
    let name = name.ok_or_else(|| format!("zulip config requires `{field}`"))?;
    secrets
        .get(&name)
        .map(SecretValue::expose_secret)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("zulip secret `{name}` is missing or empty"))
}

fn validate_alias(value: &str) -> Result<(), String> {
    let valid = !value.is_empty()
        && value.len() <= 64
        && value.as_bytes()[0].is_ascii_lowercase()
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
        });
    valid
        .then_some(())
        .ok_or_else(|| "zulip aliases must match ^[a-z][a-z0-9_-]{0,63}$".to_owned())
}

fn validate_topic(value: Option<&str>) -> Result<(), String> {
    if value.is_some_and(|value| {
        value.trim().is_empty()
            || 256 < value.len()
            || value.chars().any(tau_proto::requires_visible_escape)
    }) {
        return Err("zulip topics must be non-empty, visible, and at most 256 bytes".to_owned());
    }
    Ok(())
}

fn validate_description(value: Option<&str>) -> Result<(), String> {
    if value.is_some_and(|value| {
        120 < value.chars().count() || value.chars().any(tau_proto::requires_visible_escape)
    }) {
        return Err(
            "zulip route descriptions must contain at most 120 visible characters".to_owned(),
        );
    }
    Ok(())
}

fn normalize_api_base(site: &str) -> Result<String, String> {
    let site = site.trim_end_matches('/');
    let url = url::Url::parse(site)
        .map_err(|error| format!("zulip `site` must be a valid URL: {error}"))?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("zulip `site` must not contain userinfo, query, or fragment".to_owned());
    }
    let secure = url.scheme() == "https";
    let loopback = url.scheme() == "http"
        && url.host().is_some_and(|host| match host {
            url::Host::Domain(value) => value.eq_ignore_ascii_case("localhost"),
            url::Host::Ipv4(value) => value.is_loopback(),
            url::Host::Ipv6(value) => value.is_loopback(),
        });
    if !secure && !loopback {
        return Err("zulip `site` must use HTTPS, or HTTP only for loopback tests".to_owned());
    }
    Ok(if site.ends_with("/api/v1") {
        site.to_owned()
    } else {
        format!("{site}/api/v1")
    })
}
