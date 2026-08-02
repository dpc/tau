//! Async implementations of the four read-only tools.

mod list;
mod profile;
mod read;
mod status;

use std::str::FromStr as _;

use rostra_client::{Client, RostraId};
use tau_proto::{CborValue, Event, ToolError, ToolResult, ToolStarted};

use crate::projection::sanitize_line;
use crate::specs::{LIST_TOOL, PROFILE_TOOL, READ_TOOL, STATUS_TOOL};

/// Stable categorized tool failure.
#[derive(Debug)]
pub(crate) struct ToolFailure {
    /// Machine-stable category prefix.
    category: ToolFailureCategory,
    /// Bounded human-readable diagnostic.
    message: String,
}

/// Closed set of model-visible failure categories.
#[derive(Clone, Copy, Debug)]
pub(super) enum ToolFailureCategory {
    InvalidArgument,
    NotReady,
    NotFoundLocal,
    Timeout,
    InternalFailure,
}

impl ToolFailureCategory {
    const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidArgument => "invalid_argument",
            Self::NotReady => "not_ready",
            Self::NotFoundLocal => "not_found_local",
            Self::Timeout => "timeout",
            Self::InternalFailure => "internal_failure",
        }
    }
}

impl ToolFailure {
    /// Construct one categorized failure.
    pub(super) fn new(category: ToolFailureCategory, message: impl Into<String>) -> Self {
        Self {
            category,
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
            "local Rostra query exceeded its deadline",
        )
    }

    /// Reject work beyond the retained-query cap.
    pub(crate) fn capacity() -> Self {
        Self::new(
            ToolFailureCategory::Timeout,
            "Rostra query capacity is occupied; retry later",
        )
    }

    /// Isolate an upstream panic or impossible internal state.
    pub(crate) fn internal() -> Self {
        Self::new(
            ToolFailureCategory::InternalFailure,
            "local Rostra query failed",
        )
    }
}

type ToolTextResult = Result<String, ToolFailure>;

/// Dispatch one validated invocation to its cohesive tool module.
pub(crate) async fn dispatch(invoke: &ToolStarted, client: &Client) -> ToolTextResult {
    match invoke.tool_name.as_str() {
        STATUS_TOOL => status::handle(invoke, client).await,
        LIST_TOOL => list::handle(invoke, client).await,
        READ_TOOL => read::handle(invoke, client).await,
        PROFILE_TOOL => profile::handle(invoke, client).await,
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
        call_id: invoke.call_id.clone(),
        tool_name: invoke.tool_name.clone(),
        tool_type: tau_proto::ToolType::Function,
        display: None,
        message: format!(
            "{}: {}",
            error.category.as_str(),
            sanitize_line(&error.message, 240)
        ),
        details: None,
        originator: invoke.originator.clone(),
    })
}
