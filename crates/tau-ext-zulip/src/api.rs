use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ureq::tls as path_ureq_tls;

use crate::config::RuntimeConfig;
use crate::{HTTP_TIMEOUT, MAX_API_RESPONSE_BYTES};

/// Live Zulip queue descriptor returned by queue registration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EventQueue {
    /// Opaque queue identifier retained only inside the extension.
    pub(crate) queue_id: String,
    /// Last event ID already represented by the fresh queue tip.
    pub(crate) last_event_id: i64,
    /// Authenticated bot user ID.
    pub(crate) bot_user_id: u64,
    /// Authenticated bot full name used only for exact Markdown mention
    /// removal.
    pub(crate) bot_full_name: Option<String>,
    /// Server-advertised long-poll interval plus bounded transport grace.
    pub(crate) poll_request_timeout: Duration,
}

/// Exact native destination frozen before an outbound request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum NativeRoute {
    /// Direct message to a canonical sorted participant list, excluding the
    /// bot.
    Direct(Vec<u64>),
    /// Stream message to one exact topic.
    Stream {
        /// Stable Zulip stream ID.
        stream_id: u64,
        /// Exact Zulip topic.
        topic: String,
    },
}

/// Successful Zulip message send response.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SentMessage {
    /// Native numeric Zulip message ID.
    pub(crate) message_id: u64,
}

/// Bounded provider failure category without response bodies or credentials.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ApiError {
    /// The event queue expired and must be replaced.
    QueueExpired,
    /// Zulip requested a bounded retry delay.
    RateLimited(Duration),
    /// Authentication was refused.
    Unauthorized,
    /// The request was definitively invalid.
    InvalidRequest,
    /// The service or network failed and outcome may be unknown for mutations.
    Unavailable,
    /// A success response violated the expected bounded schema.
    MalformedResponse,
}

impl ApiError {
    /// Return a content-free user-facing diagnostic.
    pub(crate) fn diagnostic(&self) -> &'static str {
        match self {
            Self::QueueExpired => "Zulip event queue expired",
            Self::RateLimited(_) => "Zulip rate limit exceeded",
            Self::Unauthorized => "Zulip authentication failed",
            Self::InvalidRequest => "Zulip rejected the request",
            Self::Unavailable => "Zulip service is unavailable",
            Self::MalformedResponse => "Zulip returned an invalid response",
        }
    }
}

/// Small Zulip HTTP API surface used by the bridge and fake-server tests.
pub(crate) trait ZulipClient: Send + Sync + 'static {
    /// Register a fresh live-only event queue.
    fn register_queue(&self, cfg: &RuntimeConfig) -> Result<EventQueue, ApiError>;
    /// Long-poll one queue from the supplied event ID.
    fn get_events(
        &self,
        cfg: &RuntimeConfig,
        queue_id: &str,
        last_event_id: i64,
        request_timeout: Duration,
    ) -> Result<Vec<serde_json::Value>, ApiError>;
    /// Send one Markdown message to a frozen route.
    fn send_message(
        &self,
        cfg: &RuntimeConfig,
        route: &NativeRoute,
        content: &str,
    ) -> Result<SentMessage, ApiError>;
    /// Add or remove one emoji reaction on an already authorized message.
    fn react(
        &self,
        cfg: &RuntimeConfig,
        message_id: u64,
        emoji_name: &str,
        add: bool,
    ) -> Result<(), ApiError>;
}

/// Production Zulip client using HTTP Basic bot authentication.
pub(crate) struct HttpZulipClient {
    /// Reusable bounded HTTP agent.
    agent: ureq::Agent,
}

impl Default for HttpZulipClient {
    fn default() -> Self {
        let tls_config = path_ureq_tls::TlsConfig::builder()
            .root_certs(path_ureq_tls::RootCerts::PlatformVerifier)
            .build();
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(HTTP_TIMEOUT))
            .http_status_as_error(false)
            .tls_config(tls_config)
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
        }
    }
}

impl HttpZulipClient {
    fn auth(cfg: &RuntimeConfig) -> String {
        format!(
            "Basic {}",
            STANDARD.encode(format!("{}:{}", cfg.email, cfg.api_key))
        )
    }

    fn read_json(
        &self,
        mut response: ureq::http::Response<ureq::Body>,
        queue_request: bool,
    ) -> Result<serde_json::Value, ApiError> {
        let status = response.status().as_u16();
        let retry = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .map(|seconds| Duration::from_secs(seconds.min(30)));
        let text = response
            .body_mut()
            .with_config()
            .limit(MAX_API_RESPONSE_BYTES)
            .read_to_string()
            .map_err(|_| ApiError::MalformedResponse)?;
        // Provider bodies never enter diagnostics because unusual reverse proxies
        // may reflect credentials or request content.
        if status == 401 || status == 403 {
            return Err(ApiError::Unauthorized);
        }
        if status == 429 {
            return Err(ApiError::RateLimited(
                retry.unwrap_or(Duration::from_secs(1)),
            ));
        }
        if queue_request && status == 400 && text.contains("BAD_EVENT_QUEUE_ID") {
            return Err(ApiError::QueueExpired);
        }
        if 500 <= status {
            return Err(ApiError::Unavailable);
        }
        if !(200..300).contains(&status) {
            return Err(ApiError::InvalidRequest);
        }
        let value: serde_json::Value =
            serde_json::from_str(&text).map_err(|_| ApiError::MalformedResponse)?;
        if value
            .get("result")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|result| result != "success")
        {
            return Err(ApiError::InvalidRequest);
        }
        Ok(value)
    }

    fn post_form(
        &self,
        cfg: &RuntimeConfig,
        path: &str,
        form: Vec<(String, String)>,
    ) -> Result<serde_json::Value, ApiError> {
        let response = self
            .agent
            .post(&format!("{}/{path}", cfg.api_base))
            .header("Authorization", &Self::auth(cfg))
            .send_form(form)
            .map_err(|_| ApiError::Unavailable)?;
        self.read_json(response, false)
    }
}

impl ZulipClient for HttpZulipClient {
    fn register_queue(&self, cfg: &RuntimeConfig) -> Result<EventQueue, ApiError> {
        let value = self.post_form(
            cfg,
            "register",
            vec![
                (
                    "event_types".to_owned(),
                    "[\"message\",\"update_message\",\"delete_message\",\"reaction\"]".to_owned(),
                ),
                ("all_public_streams".to_owned(), "false".to_owned()),
                ("apply_markdown".to_owned(), "false".to_owned()),
            ],
        )?;
        let queue_id = value
            .get("queue_id")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty() && value.len() <= 256)
            .ok_or(ApiError::MalformedResponse)?
            .to_owned();
        let last_event_id = value
            .get("last_event_id")
            .and_then(serde_json::Value::as_i64)
            .ok_or(ApiError::MalformedResponse)?;
        let bot_user_id = value
            .get("user_id")
            .and_then(serde_json::Value::as_u64)
            .or_else(|| {
                value
                    .get("zulip_user_id")
                    .and_then(serde_json::Value::as_u64)
            })
            .ok_or(ApiError::MalformedResponse)?;
        let bot_full_name = value
            .get("full_name")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty() && value.len() <= 256)
            .map(str::to_owned);
        let longpoll_seconds = value
            .get("event_queue_longpoll_timeout_seconds")
            .and_then(serde_json::Value::as_u64)
            .filter(|seconds| (1..=600).contains(seconds))
            .ok_or(ApiError::MalformedResponse)?;
        Ok(EventQueue {
            queue_id,
            last_event_id,
            bot_user_id,
            bot_full_name,
            poll_request_timeout: Duration::from_secs(longpoll_seconds + 10),
        })
    }

    fn get_events(
        &self,
        cfg: &RuntimeConfig,
        queue_id: &str,
        last_event_id: i64,
        request_timeout: Duration,
    ) -> Result<Vec<serde_json::Value>, ApiError> {
        let response = self
            .agent
            .get(&format!("{}/events", cfg.api_base))
            .header("Authorization", &Self::auth(cfg))
            .query("queue_id", queue_id)
            .query("last_event_id", last_event_id.to_string())
            .query("dont_block", "false")
            .config()
            .timeout_global(Some(request_timeout))
            .build()
            .call()
            .map_err(|_| ApiError::Unavailable)?;
        let value = self.read_json(response, true)?;
        let events = value
            .get("events")
            .and_then(serde_json::Value::as_array)
            .ok_or(ApiError::MalformedResponse)?;
        if 4096 < events.len() {
            return Err(ApiError::QueueExpired);
        }
        Ok(events.clone())
    }

    fn send_message(
        &self,
        cfg: &RuntimeConfig,
        route: &NativeRoute,
        content: &str,
    ) -> Result<SentMessage, ApiError> {
        let mut form = vec![("content".to_owned(), content.to_owned())];
        match route {
            NativeRoute::Direct(users) => {
                form.push(("type".to_owned(), "direct".to_owned()));
                form.push((
                    "to".to_owned(),
                    serde_json::to_string(users).map_err(|_| ApiError::InvalidRequest)?,
                ));
            }
            NativeRoute::Stream { stream_id, topic } => {
                form.push(("type".to_owned(), "stream".to_owned()));
                form.push(("to".to_owned(), stream_id.to_string()));
                form.push(("topic".to_owned(), topic.clone()));
            }
        }
        let value = self.post_form(cfg, "messages", form)?;
        let message_id = value
            .get("id")
            .and_then(serde_json::Value::as_u64)
            .ok_or(ApiError::MalformedResponse)?;
        Ok(SentMessage { message_id })
    }

    fn react(
        &self,
        cfg: &RuntimeConfig,
        message_id: u64,
        emoji_name: &str,
        add: bool,
    ) -> Result<(), ApiError> {
        let url = format!("{}/messages/{message_id}/reactions", cfg.api_base);
        let form = vec![
            ("emoji_name".to_owned(), emoji_name.to_owned()),
            ("reaction_type".to_owned(), "unicode_emoji".to_owned()),
        ];
        let response = if add {
            self.agent
                .post(&url)
                .header("Authorization", &Self::auth(cfg))
                .send_form(form)
        } else {
            self.agent
                .delete(&url)
                .header("Authorization", &Self::auth(cfg))
                .query("emoji_name", emoji_name)
                .query("reaction_type", "unicode_emoji")
                .call()
        }
        .map_err(|_| ApiError::Unavailable)?;
        self.read_json(response, false)?;
        Ok(())
    }
}
