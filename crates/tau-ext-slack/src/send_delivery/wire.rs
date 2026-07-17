//! Typed Slack post composition and provider outcomes.
//!
//! This module is the provider boundary for `chat.postMessage`. It deliberately
//! keeps remote diagnostics out of user-visible errors.

use std::fmt;
use std::time::Duration;

/// Maximum Retry-After accepted from Slack.
pub(crate) const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// Minimum delay before an automatic retry.
pub(crate) const MIN_RETRY_DELAY: Duration = Duration::from_secs(1);

/// Maximum deterministic jitter added to a retry.
const MAX_RETRY_JITTER_MILLIS: u64 = 250;

/// Maximum Unicode scalar count accepted by Slack's text post surface.
const SLACK_POST_SCALAR_LIMIT: usize = 40_000;

/// Maximum bridge-owned component before Slack entity escaping.
const BRIDGE_COMPONENT_SCALAR_LIMIT: usize = 2_048;

/// Maximum final bridge-owned post size after escaping and composition.
const BRIDGE_FINAL_BYTES: usize = 8 * 1_024;

/// Typed, allowlisted diagnostic categories for Slack identity and lifecycle
/// APIs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SlackApiError {
    /// The configured credential was rejected or revoked.
    Authentication,
    /// A required Slack scope is missing.
    MissingScope,
    /// Slack throttled the request.
    RateLimited,
    /// The configured target or account is unavailable.
    TargetUnavailable,
    /// Slack denied the operation.
    PermissionDenied,
    /// Slack rejected the bounded request.
    InvalidRequest,
    /// The response did not match the documented shape.
    MalformedResponse,
    /// The transport timed out.
    TransportTimeout,
    /// The transport could not establish a connection.
    TransportConnect,
    /// TLS validation or negotiation failed.
    TransportTls,
    /// The established transport failed.
    Transport,
    /// Slack failed internally or returned an unclassified response.
    RemoteFailure,
}

impl fmt::Display for SlackApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Authentication => "Slack authentication failed",
            Self::MissingScope => "Slack is missing a required scope",
            Self::RateLimited => "Slack rate limited the request",
            Self::TargetUnavailable => "Slack target is unavailable",
            Self::PermissionDenied => "Slack permission was denied",
            Self::InvalidRequest => "Slack rejected the request",
            Self::MalformedResponse => "Slack returned a malformed response",
            Self::TransportTimeout => "Slack transport timed out",
            Self::TransportConnect => "Slack transport connection failed",
            Self::TransportTls => "Slack TLS transport failed",
            Self::Transport => "Slack transport failed",
            Self::RemoteFailure => "Slack service failed",
        })
    }
}

/// Typed send failure category safe for logs and model-visible tool errors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SendFailureCategory {
    /// The configured credential was rejected or revoked.
    Authentication,
    /// A required Slack scope is missing.
    MissingScope,
    /// Slack denied the destination or operation.
    PermissionDenied,
    /// The exact destination is unavailable.
    TargetUnavailable,
    /// Slack definitively rejected the bounded request.
    InvalidRequest,
    /// The transport timed out.
    Timeout,
    /// The transport could not establish a connection.
    Connect,
    /// TLS validation or negotiation failed.
    Tls,
    /// The transport failed before a definitive response.
    Transport,
    /// Slack failed internally.
    ServiceUnavailable,
    /// Slack required a retry outside the bounded call horizon.
    RateLimited,
    /// The response could not be classified without risking omission.
    MalformedResponse,
    /// A successful response named a route other than the frozen route.
    ConflictingRoute,
    /// The delivery worker could not be started.
    WorkerUnavailable,
}

impl SendFailureCategory {
    /// Return a stable low-cardinality tracing label.
    pub(crate) fn trace_label(self) -> &'static str {
        match self {
            Self::Authentication => "authentication",
            Self::MissingScope => "missing_scope",
            Self::PermissionDenied => "permission_denied",
            Self::TargetUnavailable => "target_unavailable",
            Self::InvalidRequest => "invalid_request",
            Self::Timeout => "timeout",
            Self::Connect => "connect",
            Self::Tls => "tls",
            Self::Transport => "transport",
            Self::ServiceUnavailable => "service_unavailable",
            Self::RateLimited => "rate_limited",
            Self::MalformedResponse => "malformed_response",
            Self::ConflictingRoute => "conflicting_route",
            Self::WorkerUnavailable => "worker_unavailable",
        }
    }
}

impl fmt::Display for SendFailureCategory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Authentication => "Slack authentication failed",
            Self::MissingScope => "Slack is missing a required scope",
            Self::PermissionDenied => "Slack permission was denied",
            Self::TargetUnavailable => "Slack destination is unavailable",
            Self::InvalidRequest => "Slack rejected the message",
            Self::Timeout => "Slack delivery timed out",
            Self::Connect => "Slack delivery connection failed",
            Self::Tls => "Slack delivery TLS failed",
            Self::Transport => "Slack delivery transport failed",
            Self::ServiceUnavailable => "Slack service is unavailable",
            Self::RateLimited => "Slack rate limit exceeds the retry horizon",
            Self::MalformedResponse => "Slack delivery outcome is unknown",
            Self::ConflictingRoute => "Slack returned conflicting route metadata",
            Self::WorkerUnavailable => "Slack delivery worker is unavailable",
        })
    }
}

/// Typed result of one `chat.postMessage` attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PostAttemptOutcome<T> {
    /// Slack returned a validated accepted post.
    Accepted(T),
    /// Slack definitively rejected the request and retrying is unsafe or
    /// useless.
    DefinitiveFailure(SendFailureCategory),
    /// The request may have produced a remote copy and is eligible for one
    /// retry.
    OutcomeUnknown(SendFailureCategory),
    /// Slack requested a bounded retry delay.
    RateLimited(Duration),
}

/// Non-success result used by parsers/classifiers that cannot produce success.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PostAttemptFailure {
    /// Slack definitively rejected the request.
    Definitive(SendFailureCategory),
    /// A remote copy may exist.
    OutcomeUnknown(SendFailureCategory),
    /// Slack requested a bounded retry delay.
    RateLimited(Duration),
}

impl<T> PostAttemptOutcome<T> {
    /// Return the safe tracing category for this result.
    pub(crate) fn trace_label(&self) -> &'static str {
        match self {
            Self::Accepted(_) => "ok",
            Self::DefinitiveFailure(category) | Self::OutcomeUnknown(category) => {
                category.trace_label()
            }
            Self::RateLimited(_) => "rate_limited",
        }
    }
}

/// Error returned while composing a safe outbound post.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PostCompositionError {
    /// Agent text contained raw Slack native-control markup.
    NativeControlMarkup,
    /// Text exceeded Slack's final scalar limit.
    TooLong,
    /// An internal source mention was not an exact U/W user id.
    InvalidSourceMention,
}

/// Validated internal-only Slack source mention.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InternalSourceMention(String);

impl InternalSourceMention {
    /// Validate one exact U/W Slack user id for generated markup.
    pub(crate) fn new(user_id: &str) -> Result<Self, PostCompositionError> {
        valid_source_user_id(user_id)
            .then(|| Self(user_id.to_owned()))
            .ok_or(PostCompositionError::InvalidSourceMention)
    }

    /// Return the validated native id for internal composition only.
    fn user_id(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PostCompositionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NativeControlMarkup => {
                "message contains unsupported raw Slack mention or channel markup"
            }
            Self::TooLong => "message exceeds Slack's final post limit",
            Self::InvalidSourceMention => "internal Slack source mention is invalid",
        })
    }
}

/// Opaque semantic post mode with fully composed safe text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SlackPostMode {
    /// Validated and bounded final post text.
    text: String,
    /// Whether Slack may interpret ordinary mrkdwn.
    mrkdwn: bool,
}

impl SlackPostMode {
    /// Compose agent text and optionally prepend one internally generated
    /// source mention.
    ///
    /// Agent text is always checked before the generated mention is added, so
    /// model text can never smuggle another `<@`, `<!`, or `<#` control.
    pub(crate) fn agent(
        text: String,
        source_mention: Option<&InternalSourceMention>,
    ) -> Result<Self, PostCompositionError> {
        if contains_native_control(&text) {
            return Err(PostCompositionError::NativeControlMarkup);
        }
        let text = if let Some(source_mention) = source_mention {
            let user_id = source_mention.user_id();
            format!("<@{user_id}> {text}")
        } else {
            text
        };
        if text.chars().count() > SLACK_POST_SCALAR_LIMIT {
            return Err(PostCompositionError::TooLong);
        }
        Ok(Self { text, mrkdwn: true })
    }

    /// Compose bounded bridge-owned literal text.
    pub(crate) fn bridge_literal(component: &str) -> Self {
        let single_line = component
            .chars()
            .take(BRIDGE_COMPONENT_SCALAR_LIMIT)
            .map(|character| {
                if character.is_control() {
                    ' '
                } else {
                    character
                }
            })
            .collect::<String>();
        let escaped = escape_slack_entities(&single_line);
        let bounded = truncate_utf8_bytes(&escaped, BRIDGE_FINAL_BYTES);
        Self {
            text: truncate_scalars(&bounded, SLACK_POST_SCALAR_LIMIT),
            mrkdwn: false,
        }
    }

    /// Return the final post text.
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// Return whether Slack mrkdwn parsing is enabled.
    pub(crate) fn mrkdwn(&self) -> bool {
        self.mrkdwn
    }
}

/// Frozen exact `chat.postMessage` wire body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FrozenPostBody {
    /// Exact serialized JSON reused by both attempts.
    wire_json: String,
    /// Exact expected response conversation.
    channel_id: String,
    /// Exact expected response thread root.
    thread_ts: Option<String>,
}

impl FrozenPostBody {
    /// Serialize one semantic post mode and exact route into the final body.
    pub(crate) fn new(channel_id: &str, thread_ts: Option<&str>, mode: &SlackPostMode) -> Self {
        let mut body = serde_json::json!({
            "channel": channel_id,
            "text": mode.text(),
            "mrkdwn": mode.mrkdwn(),
            "link_names": false
        });
        if let Some(thread_ts) = thread_ts {
            body["thread_ts"] = serde_json::Value::String(thread_ts.to_owned());
        }
        Self {
            wire_json: body.to_string(),
            channel_id: channel_id.to_owned(),
            thread_ts: thread_ts.map(str::to_owned),
        }
    }

    /// Return the exact serialized JSON body.
    pub(crate) fn wire_json(&self) -> &str {
        &self.wire_json
    }

    /// Return the exact frozen destination conversation.
    pub(crate) fn channel_id(&self) -> &str {
        &self.channel_id
    }

    /// Return the exact frozen thread root.
    pub(crate) fn thread_ts(&self) -> Option<&str> {
        self.thread_ts.as_deref()
    }
}

/// Parse and clamp Slack Retry-After without retaining the raw header.
pub(crate) fn parse_retry_after(value: Option<&str>) -> Duration {
    let Some(value) = value else {
        return MIN_RETRY_DELAY;
    };
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return MIN_RETRY_DELAY;
    }
    value.parse::<u64>().map_or(MAX_RETRY_AFTER, |seconds| {
        Duration::from_secs(seconds).clamp(MIN_RETRY_DELAY, MAX_RETRY_AFTER)
    })
}

/// Return deterministic bounded jitter for one frozen call and channel.
pub(crate) fn retry_jitter(call_id: &str, channel_id: &str) -> Duration {
    let hash = call_id
        .bytes()
        .chain([0xff])
        .chain(channel_id.bytes())
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            hash.wrapping_mul(0x0000_0100_0000_01b3) ^ u64::from(byte)
        });
    Duration::from_millis(hash % (MAX_RETRY_JITTER_MILLIS + 1))
}

/// Add bounded jitter to a provider delay.
pub(crate) fn retry_delay(base: Duration, call_id: &str, channel_id: &str) -> Duration {
    base.max(MIN_RETRY_DELAY)
        .saturating_add(retry_jitter(call_id, channel_id))
        .min(MAX_RETRY_AFTER)
}

/// Map a Slack API error code into a safe general API category.
pub(crate) fn classify_api_error(code: Option<&str>) -> SlackApiError {
    match code {
        Some("invalid_auth" | "not_authed" | "account_inactive" | "token_revoked") => {
            SlackApiError::Authentication
        }
        Some("missing_scope") => SlackApiError::MissingScope,
        Some("ratelimited") => SlackApiError::RateLimited,
        Some("channel_not_found" | "user_not_found" | "not_found" | "is_archived") => {
            SlackApiError::TargetUnavailable
        }
        Some("not_in_channel" | "restricted_action" | "missing_permission") => {
            SlackApiError::PermissionDenied
        }
        Some("invalid_arguments" | "invalid_arg_name" | "invalid_array_arg") => {
            SlackApiError::InvalidRequest
        }
        Some("fatal_error" | "internal_error" | "request_timeout" | "service_unavailable") => {
            SlackApiError::RemoteFailure
        }
        Some(_) | None => SlackApiError::RemoteFailure,
    }
}

/// Map one Slack post API code into retry and ambiguity semantics.
pub(crate) fn classify_post_api_error(code: Option<&str>) -> PostAttemptFailure {
    match code {
        Some("fatal_error" | "internal_error" | "request_timeout" | "service_unavailable") => {
            PostAttemptFailure::OutcomeUnknown(SendFailureCategory::ServiceUnavailable)
        }
        Some("invalid_auth" | "not_authed" | "account_inactive" | "token_revoked") => {
            PostAttemptFailure::Definitive(SendFailureCategory::Authentication)
        }
        Some("missing_scope") => PostAttemptFailure::Definitive(SendFailureCategory::MissingScope),
        Some("channel_not_found" | "is_archived" | "not_found") => {
            PostAttemptFailure::Definitive(SendFailureCategory::TargetUnavailable)
        }
        Some("not_in_channel" | "restricted_action" | "missing_permission") => {
            PostAttemptFailure::Definitive(SendFailureCategory::PermissionDenied)
        }
        Some("invalid_arguments" | "invalid_arg_name" | "invalid_array_arg" | "msg_too_long") => {
            PostAttemptFailure::Definitive(SendFailureCategory::InvalidRequest)
        }
        Some("ratelimited") => PostAttemptFailure::RateLimited(MIN_RETRY_DELAY),
        Some(_) | None => {
            PostAttemptFailure::OutcomeUnknown(SendFailureCategory::MalformedResponse)
        }
    }
}

fn contains_native_control(text: &str) -> bool {
    text.contains("<@") || text.contains("<!") || text.contains("<#")
}

fn valid_source_user_id(user_id: &str) -> bool {
    matches!(user_id.as_bytes().first(), Some(b'U' | b'W'))
        && (2..=32).contains(&user_id.len())
        && user_id
            .bytes()
            .skip(1)
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
}

fn escape_slack_entities(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn truncate_scalars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

fn truncate_utf8_bytes(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}
