//! Async implementations of local read and authenticated-write tools.

mod list;
mod profile;
mod read;
mod status;
pub(crate) mod write;

use std::str::FromStr as _;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use rostra_client::{Client, RostraId};
use rostra_core::id::RostraIdSecretKey;
use tau_proto::{CborValue, Event, ToolError, ToolResult, ToolStarted};

use crate::post_rate_limit::{PostRateLimit, PostRateLimitWindow};
use crate::projection::sanitize_line;
use crate::specs::{
    FOLLOW_TOOL, LIST_TOOL, POST_TOOL, PROFILE_TOOL, PROFILE_UPDATE_TOOL, REACT_TOOL, READ_TOOL,
    STATUS_TOOL, UNFOLLOW_TOOL, VOTE_TOOL,
};

/// Stable categorized tool failure.
#[derive(Debug)]
pub(crate) struct ToolFailure {
    /// Failure category and any category-specific structured metadata.
    kind: ToolFailureKind,
    /// Bounded human-readable diagnostic.
    message: String,
}

/// One failure's category and category-specific structured metadata.
#[derive(Clone, Copy, Debug)]
enum ToolFailureKind {
    /// A failure category without structured fields.
    Category(ToolFailureCategory),
    /// A full runtime post quota with its exact retry time.
    RateLimited {
        /// Whole seconds until a post-like attempt can retry.
        retry_after_seconds: u64,
    },
}

/// Closed set of model-visible failure categories.
#[derive(Clone, Copy, Debug)]
pub(super) enum ToolFailureCategory {
    InvalidArgument,
    NotReady,
    NotFoundLocal,
    StorageFailure,
    Timeout,
    InternalFailure,
}

impl ToolFailureCategory {
    const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidArgument => "invalid_argument",
            Self::NotReady => "not_ready",
            Self::NotFoundLocal => "not_found_local",
            Self::StorageFailure => "storage_failure",
            Self::Timeout => "timeout",
            Self::InternalFailure => "internal_failure",
        }
    }
}

impl ToolFailureKind {
    /// Return the stable model-visible category spelling.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Category(category) => category.as_str(),
            Self::RateLimited { .. } => "rate_limited",
        }
    }

    /// Return structured metadata only for the category that defines it.
    fn details(self) -> Option<CborValue> {
        let Self::RateLimited {
            retry_after_seconds,
        } = self
        else {
            return None;
        };
        Some(rate_limit_details(retry_after_seconds))
    }
}

impl ToolFailure {
    /// Construct one categorized failure.
    pub(super) fn new(category: ToolFailureCategory, message: impl Into<String>) -> Self {
        Self {
            kind: ToolFailureKind::Category(category),
            message: message.into(),
        }
    }

    /// Reject malformed arguments.
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::new(ToolFailureCategory::InvalidArgument, message)
    }

    /// Report use before successful configuration.
    pub(crate) fn not_ready() -> Self {
        Self::new(ToolFailureCategory::NotReady, "Rostra client is not ready")
    }

    /// Suppress a late result after the model-visible deadline.
    pub(crate) fn timeout() -> Self {
        Self::new(
            ToolFailureCategory::Timeout,
            "local Rostra operation exceeded its deadline; a signed write may still have completed",
        )
    }

    /// Reject work beyond the retained-query cap.
    pub(crate) fn capacity() -> Self {
        Self::new(
            ToolFailureCategory::Timeout,
            "Rostra operation capacity is occupied; retry later",
        )
    }

    /// Isolate an upstream panic or impossible internal state.
    pub(crate) fn internal() -> Self {
        Self::new(
            ToolFailureCategory::InternalFailure,
            "local Rostra query failed",
        )
    }

    /// Report a failed local signed-event transaction.
    pub(crate) fn storage() -> Self {
        Self::new(
            ToolFailureCategory::StorageFailure,
            "local Rostra signed-event transaction failed",
        )
    }

    /// Report a full configured rolling post quota.
    pub(crate) fn rate_limited(retry_after_seconds: u64) -> Self {
        Self {
            kind: ToolFailureKind::RateLimited {
                retry_after_seconds,
            },
            message: format!("post rate limit reached; retry after {retry_after_seconds} seconds"),
        }
    }
}

type ToolTextResult = Result<String, ToolFailure>;

/// Dispatch one validated invocation to its cohesive tool module.
pub(crate) async fn dispatch(
    invoke: &ToolStarted,
    client: &Client,
    identity_secret: Option<RostraIdSecretKey>,
    write_lock: Arc<tokio::sync::Mutex<()>>,
    post_rate_limit: PostRateLimit,
    post_rate_limit_window: Arc<Mutex<PostRateLimitWindow>>,
    publication_admitted: Arc<AtomicBool>,
) -> ToolTextResult {
    match invoke.tool_name.as_str() {
        STATUS_TOOL => status::handle(invoke, client).await,
        LIST_TOOL => list::handle(invoke, client).await,
        READ_TOOL => read::handle(invoke, client).await,
        PROFILE_TOOL => profile::handle(invoke, client).await,
        POST_TOOL | REACT_TOOL | FOLLOW_TOOL | UNFOLLOW_TOOL | PROFILE_UPDATE_TOOL | VOTE_TOOL => {
            let secret = identity_secret.ok_or_else(ToolFailure::not_ready)?;
            write::handle(
                invoke,
                client,
                secret,
                write_lock,
                post_rate_limit,
                post_rate_limit_window,
                publication_admitted,
            )
            .await
        }
        _ => Err(ToolFailure::invalid("unknown Rostra tool")),
    }
}

/// Parse one canonical public Rostra identity.
pub(super) fn parse_identity(value: &str) -> Result<RostraId, ToolFailure> {
    RostraId::from_str(value).map_err(|_| ToolFailure::invalid("identity is not a valid Rostra id"))
}

/// Decode one strict argument object from protocol CBOR.
pub(super) fn decode_args<T: serde::de::DeserializeOwned>(
    arguments: &CborValue,
) -> Result<T, ToolFailure> {
    let value = serde_json::to_value(arguments)
        .map_err(|_| ToolFailure::invalid("arguments are not an object"))?;
    serde_json::from_value(value)
        .map_err(|_| ToolFailure::invalid("arguments do not match the tool schema"))
}

/// Build a successful terminal event for one invocation.
pub(crate) fn tool_result(invoke: &ToolStarted, text: String) -> Event {
    Event::ToolResult(ToolResult {
        presentation: Default::default(),
        call_id: invoke.call_id.clone(),
        tool_name: invoke.tool_name.clone(),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text(text),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator: invoke.originator.clone(),
    })
}

/// Build a categorized failed terminal event for one invocation.
pub(crate) fn tool_error(invoke: &ToolStarted, error: ToolFailure) -> Event {
    Event::ToolError(ToolError {
        presentation: Default::default(),
        call_id: invoke.call_id.clone(),
        tool_name: invoke.tool_name.clone(),
        tool_type: tau_proto::ToolType::Function,
        display: None,
        message: format!(
            "{}: {}",
            error.kind.as_str(),
            sanitize_line(&error.message, 240)
        ),
        details: error.kind.details(),
        originator: invoke.originator.clone(),
    })
}

/// Encode the fixed structured details for a full runtime post quota.
fn rate_limit_details(retry_after_seconds: u64) -> CborValue {
    CborValue::Map(vec![
        (
            CborValue::Text("category".to_owned()),
            CborValue::Text("rate_limited".to_owned()),
        ),
        (
            CborValue::Text("retry_after_seconds".to_owned()),
            CborValue::Integer(retry_after_seconds.into()),
        ),
    ])
}
