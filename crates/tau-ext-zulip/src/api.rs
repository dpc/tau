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

/// One bounded ascending Zulip message-history page.
pub(crate) struct MessagePage {
    /// Messages returned after the requested anchor.
    pub(crate) messages: Vec<serde_json::Value>,
    /// Whether this page reaches the server's current newest message.
    pub(crate) found_newest: bool,
}

/// Bounded provider failure category without response bodies or credentials.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ApiError {
    /// The event queue expired and must be replaced.
    QueueExpired,
    /// Zulip requested a bounded retry delay.
    RateLimited {
        /// The bounded delay requested by Zulip.
        retry: Duration,
        /// Safe rejection metadata when Zulip returned an HTTP response.
        rejection: Option<RemoteRejection>,
    },
    /// Authentication was refused.
    Unauthorized {
        /// Safe rejection metadata when Zulip returned an HTTP response.
        rejection: Option<RemoteRejection>,
    },
    /// The request was definitively invalid.
    InvalidRequest {
        /// Safe rejection metadata when Zulip returned an HTTP response.
        rejection: Option<RemoteRejection>,
    },
    /// The service or network failed and outcome may be unknown for mutations.
    Unavailable {
        /// Safe rejection metadata when Zulip returned an HTTP response.
        rejection: Option<RemoteRejection>,
    },
    /// A success response violated the expected bounded schema.
    MalformedResponse,
}

impl ApiError {
    /// Return a content-free user-facing diagnostic.
    pub(crate) fn diagnostic(&self) -> String {
        match self {
            Self::QueueExpired => "Zulip event queue expired".to_owned(),
            Self::RateLimited {
                rejection: Some(rejection),
                ..
            } => format!("Zulip rate limit exceeded ({})", rejection.detail()),
            Self::RateLimited {
                rejection: None, ..
            } => "Zulip rate limit exceeded".to_owned(),
            Self::Unauthorized {
                rejection: Some(rejection),
            } => {
                format!("Zulip authentication failed ({})", rejection.detail())
            }
            Self::Unauthorized { rejection: None } => "Zulip authentication failed".to_owned(),
            Self::InvalidRequest {
                rejection: Some(rejection),
            } => {
                format!("Zulip rejected the request ({})", rejection.detail())
            }
            Self::InvalidRequest { rejection: None } => "Zulip rejected the request".to_owned(),
            Self::Unavailable {
                rejection: Some(rejection),
            } => {
                format!("Zulip service is unavailable ({})", rejection.detail())
            }
            Self::Unavailable { rejection: None } => "Zulip service is unavailable".to_owned(),
            Self::MalformedResponse => "Zulip returned an invalid response".to_owned(),
        }
    }

    /// Build a safely classified startup rejection without retaining remote
    /// text.
    #[cfg(test)]
    pub(crate) fn rejected_startup_request(
        operation: RejectedOperation,
        status: u16,
        retry: Option<Duration>,
        code: &str,
    ) -> Self {
        let rejection = Some(RemoteRejection::new(operation, status, code));
        classify_http_rejection(status, retry, rejection.clone())
            .unwrap_or(Self::InvalidRequest { rejection })
    }

    /// Return the generic category used when no remote response was available.
    pub(crate) fn unavailable() -> Self {
        Self::Unavailable { rejection: None }
    }

    /// Return a generic local validation failure without remote response data.
    pub(crate) fn invalid_request() -> Self {
        Self::InvalidRequest { rejection: None }
    }
}

/// Startup requests whose rejection diagnostics name an operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RejectedOperation {
    /// The authenticated-bot identity request.
    UsersMe,
    /// Resolve a configured channel name to its native ID.
    StreamId,
    /// Subscribe the bot to configured all-message channels.
    Subscribe,
    /// The event-queue registration request.
    Register,
}

impl RejectedOperation {
    /// Return the stable, data-free operation label.
    fn label(self) -> &'static str {
        match self {
            Self::UsersMe => "users_me",
            Self::StreamId => "get_stream_id",
            Self::Subscribe => "subscribe",
            Self::Register => "register",
        }
    }
}

/// Meaning of an API response while selecting safe failure handling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResponseContext {
    /// A request without special response handling.
    Ordinary,
    /// A long-poll response that may report an expired event queue.
    QueuePoll,
    /// A startup request whose rejection receives a bounded diagnostic.
    Startup(RejectedOperation),
}

/// Safe metadata retained from a remote rejection for a startup diagnostic.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RemoteRejection {
    /// Stable local name for the rejected request.
    operation: RejectedOperation,
    /// HTTP status returned by the remote server.
    status: u16,
    /// Validated bounded Zulip machine error code, or `unknown`.
    code: String,
}

impl RemoteRejection {
    /// Retain only a bounded safe code from a remote rejection.
    fn new(operation: RejectedOperation, status: u16, code: &str) -> Self {
        Self {
            operation,
            status,
            code: sanitize_zulip_error_code(code),
        }
    }

    /// Render the fixed-shape diagnostic without remote response content.
    fn detail(&self) -> String {
        format!(
            "{}, HTTP {}, code {}",
            self.operation.label(),
            self.status,
            self.code
        )
    }
}

/// Maximum number of ASCII bytes accepted from a Zulip machine error code.
const MAX_ZULIP_ERROR_CODE_BYTES: usize = 64;

/// Small Zulip HTTP API surface used by the bridge and fake-server tests.
pub(crate) trait ZulipClient: Send + Sync + 'static {
    /// Resolve one configured Zulip channel name to its private native ID.
    fn resolve_stream_id(&self, cfg: &RuntimeConfig, name: &str) -> Result<u64, ApiError>;
    /// Subscribe the bot to the named configured all-message channels.
    fn subscribe(&self, cfg: &RuntimeConfig, names: &[String]) -> Result<(), ApiError>;
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
    /// Fetch currently queued events without waiting for a future event.
    fn get_events_now(
        &self,
        cfg: &RuntimeConfig,
        queue_id: &str,
        last_event_id: i64,
    ) -> Result<Vec<serde_json::Value>, ApiError>;
    /// Fetch one bounded ascending page of messages newer than a native ID.
    fn get_messages_after(
        &self,
        cfg: &RuntimeConfig,
        after: u64,
        limit: usize,
    ) -> Result<MessagePage, ApiError>;
    /// Fetch the newest currently visible native message ID for first-use
    /// baselining.
    fn newest_message_id(&self, cfg: &RuntimeConfig) -> Result<Option<u64>, ApiError>;
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
        context: ResponseContext,
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
            .ok();
        // Provider bodies never enter diagnostics because unusual reverse proxies
        // may reflect credentials or request content.
        let remote_rejection = |code: String| match context {
            ResponseContext::Startup(operation) => {
                Some(RemoteRejection::new(operation, status, &code))
            }
            ResponseContext::Ordinary | ResponseContext::QueuePoll => None,
        };
        let Some(text) = text else {
            return Err(classify_http_rejection(
                status,
                retry,
                remote_rejection("unknown".to_owned()),
            )
            .unwrap_or(ApiError::MalformedResponse));
        };
        if context == ResponseContext::QueuePoll
            && status == 400
            && text.contains("BAD_EVENT_QUEUE_ID")
        {
            return Err(ApiError::QueueExpired);
        }
        if let Some(error) =
            classify_http_rejection(status, retry, remote_rejection(zulip_error_code(&text)))
        {
            return Err(error);
        }
        let value: serde_json::Value =
            serde_json::from_str(&text).map_err(|_| ApiError::MalformedResponse)?;
        if value
            .get("result")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|result| result != "success")
        {
            return Err(ApiError::InvalidRequest {
                rejection: remote_rejection(zulip_error_code(&text)),
            });
        }
        Ok(value)
    }

    fn post_form(
        &self,
        cfg: &RuntimeConfig,
        path: &str,
        form: Vec<(String, String)>,
        context: ResponseContext,
    ) -> Result<serde_json::Value, ApiError> {
        let response = self
            .agent
            .post(&format!("{}/{path}", cfg.api_base))
            .header("Authorization", &Self::auth(cfg))
            .send_form(form)
            .map_err(|_| ApiError::unavailable())?;
        self.read_json(response, context)
    }

    fn current_user_id(&self, cfg: &RuntimeConfig) -> Result<u64, ApiError> {
        let response = self
            .agent
            .get(&format!("{}/users/me", cfg.api_base))
            .header("Authorization", &Self::auth(cfg))
            .call()
            .map_err(|_| ApiError::unavailable())?;
        let value = self.read_json(
            response,
            ResponseContext::Startup(RejectedOperation::UsersMe),
        )?;
        let user_id = value
            .get("user_id")
            .and_then(serde_json::Value::as_u64)
            .ok_or(ApiError::MalformedResponse)?;
        Ok(user_id)
    }

    fn get_messages(
        &self,
        cfg: &RuntimeConfig,
        anchor: &str,
        num_before: usize,
        num_after: usize,
        include_anchor: bool,
        max_messages: usize,
    ) -> Result<MessagePage, ApiError> {
        let request = self
            .agent
            .get(&format!("{}/messages", cfg.api_base))
            .header("Authorization", &Self::auth(cfg))
            .query("anchor", anchor)
            .query("num_before", num_before.to_string())
            .query("num_after", num_after.to_string())
            .query("include_anchor", include_anchor.to_string())
            .query("apply_markdown", "false");
        let response = request.call().map_err(|_| ApiError::unavailable())?;
        let value = self.read_json(response, ResponseContext::Ordinary)?;
        let messages = value
            .get("messages")
            .and_then(serde_json::Value::as_array)
            .ok_or(ApiError::MalformedResponse)?;
        if max_messages < messages.len() {
            return Err(ApiError::MalformedResponse);
        }
        let found_newest = value
            .get("found_newest")
            .and_then(serde_json::Value::as_bool)
            .ok_or(ApiError::MalformedResponse)?;
        Ok(MessagePage {
            messages: messages.clone(),
            found_newest,
        })
    }
}

impl ZulipClient for HttpZulipClient {
    fn resolve_stream_id(&self, cfg: &RuntimeConfig, name: &str) -> Result<u64, ApiError> {
        let response = self
            .agent
            .get(&format!("{}/get_stream_id", cfg.api_base))
            .header("Authorization", &Self::auth(cfg))
            .query("stream", name)
            .call()
            .map_err(|_| ApiError::unavailable())?;
        let value = self.read_json(
            response,
            ResponseContext::Startup(RejectedOperation::StreamId),
        )?;
        value
            .get("stream_id")
            .and_then(serde_json::Value::as_u64)
            .filter(|id| *id != 0)
            .ok_or(ApiError::MalformedResponse)
    }

    fn subscribe(&self, cfg: &RuntimeConfig, names: &[String]) -> Result<(), ApiError> {
        if names.is_empty() {
            return Ok(());
        }
        self.post_form(
            cfg,
            "users/me/subscriptions",
            vec![
                (
                    "subscriptions".to_owned(),
                    serde_json::to_string(
                        &names
                            .iter()
                            .map(|name| serde_json::json!({"name": name}))
                            .collect::<Vec<_>>(),
                    )
                    .map_err(|_| ApiError::invalid_request())?,
                ),
                ("authorization_errors_fatal".to_owned(), "true".to_owned()),
            ],
            ResponseContext::Startup(RejectedOperation::Subscribe),
        )?;
        Ok(())
    }

    fn register_queue(&self, cfg: &RuntimeConfig) -> Result<EventQueue, ApiError> {
        let bot_user_id = self.current_user_id(cfg)?;
        let value = self.post_form(
            cfg,
            "register",
            vec![
                (
                    "event_types".to_owned(),
                    "[\"message\",\"update_message\",\"delete_message\",\"reaction\"]".to_owned(),
                ),
                // Zulip includes its recommended long-poll timeout in the
                // initial `realm` state. Fetch identity separately to avoid
                // loading the complete realm user directory.
                ("fetch_event_types".to_owned(), "[\"realm\"]".to_owned()),
                ("all_public_streams".to_owned(), "false".to_owned()),
                ("apply_markdown".to_owned(), "false".to_owned()),
                (
                    "client_capabilities".to_owned(),
                    // Zulip's schema keeps this historical member required even
                    // when the client does not use null notification settings.
                    r#"{"notification_settings_null":false,"empty_topic_name":true}"#.to_owned(),
                ),
            ],
            ResponseContext::Startup(RejectedOperation::Register),
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
        let longpoll_seconds = value
            .get("event_queue_longpoll_timeout_seconds")
            .and_then(serde_json::Value::as_u64)
            .filter(|seconds| (1..=600).contains(seconds))
            .ok_or(ApiError::MalformedResponse)?;
        Ok(EventQueue {
            queue_id,
            last_event_id,
            bot_user_id,
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
            .map_err(|_| ApiError::unavailable())?;
        let value = self.read_json(response, ResponseContext::QueuePoll)?;
        let events = value
            .get("events")
            .and_then(serde_json::Value::as_array)
            .ok_or(ApiError::MalformedResponse)?;
        if 4096 < events.len() {
            return Err(ApiError::QueueExpired);
        }
        Ok(events.clone())
    }

    fn get_events_now(
        &self,
        cfg: &RuntimeConfig,
        queue_id: &str,
        last_event_id: i64,
    ) -> Result<Vec<serde_json::Value>, ApiError> {
        let response = self
            .agent
            .get(&format!("{}/events", cfg.api_base))
            .header("Authorization", &Self::auth(cfg))
            .query("queue_id", queue_id)
            .query("last_event_id", last_event_id.to_string())
            .query("dont_block", "true")
            .call()
            .map_err(|_| ApiError::unavailable())?;
        let value = self.read_json(response, ResponseContext::QueuePoll)?;
        let events = value
            .get("events")
            .and_then(serde_json::Value::as_array)
            .filter(|events| events.len() <= 4096)
            .ok_or(ApiError::MalformedResponse)?;
        Ok(events.clone())
    }

    fn get_messages_after(
        &self,
        cfg: &RuntimeConfig,
        after: u64,
        limit: usize,
    ) -> Result<MessagePage, ApiError> {
        if limit == 0 || 100 < limit {
            return Err(ApiError::MalformedResponse);
        }
        self.get_messages(cfg, after.to_string().as_str(), 0, limit, false, limit)
    }

    fn newest_message_id(&self, cfg: &RuntimeConfig) -> Result<Option<u64>, ApiError> {
        let page = self.get_messages(cfg, "newest", 1, 0, true, 2)?;
        if !page.found_newest {
            return Err(ApiError::MalformedResponse);
        }
        let mut newest = None;
        for message in page.messages {
            let message_id = message
                .get("id")
                .and_then(serde_json::Value::as_u64)
                .ok_or(ApiError::MalformedResponse)?;
            if newest.is_some_and(|previous| message_id <= previous) {
                return Err(ApiError::MalformedResponse);
            }
            newest = Some(message_id);
        }
        Ok(newest)
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
                    serde_json::to_string(users)
                        .map_err(|_| ApiError::InvalidRequest { rejection: None })?,
                ));
            }
            NativeRoute::Stream { stream_id, topic } => {
                form.push(("type".to_owned(), "stream".to_owned()));
                form.push(("to".to_owned(), stream_id.to_string()));
                form.push(("topic".to_owned(), topic.clone()));
            }
        }
        let value = self.post_form(cfg, "messages", form, ResponseContext::Ordinary)?;
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
        .map_err(|_| ApiError::unavailable())?;
        self.read_json(response, ResponseContext::Ordinary)?;
        Ok(())
    }
}

/// Preserve the existing category for a non-success HTTP response.
fn classify_http_rejection(
    status: u16,
    retry: Option<Duration>,
    rejection: Option<RemoteRejection>,
) -> Option<ApiError> {
    if status == 401 || status == 403 {
        return Some(ApiError::Unauthorized { rejection });
    }
    if status == 429 {
        return Some(ApiError::RateLimited {
            retry: retry.unwrap_or(Duration::from_secs(1)),
            rejection,
        });
    }
    if 500 <= status {
        return Some(ApiError::Unavailable { rejection });
    }
    if !(200..300).contains(&status) {
        return Some(ApiError::InvalidRequest { rejection });
    }
    None
}

/// Extract a bounded machine error code without retaining remote response text.
fn zulip_error_code(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("code")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .map_or_else(
            || "unknown".to_owned(),
            |code| sanitize_zulip_error_code(&code),
        )
}

/// Replace an unsafe Zulip machine code with the fixed fallback.
fn sanitize_zulip_error_code(code: &str) -> String {
    if !code.is_empty()
        && code.len() < MAX_ZULIP_ERROR_CODE_BYTES + 1
        && code
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return code.to_owned();
    }
    "unknown".to_owned()
}
