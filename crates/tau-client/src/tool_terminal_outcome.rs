/// Typed terminal outcome produced by an extension tool implementation.
///
/// Pure dispatch code may continue to construct canonical-shaped result, error,
/// or cancellation DTOs without selecting a peer wire event name. Client output
/// adapters convert this enum into the corresponding transient report.
#[derive(Clone, Debug, PartialEq)]
pub enum ToolTerminalOutcome {
    /// Successful terminal result.
    Result(tau_proto::ToolResult),
    /// Failed terminal result.
    Failure(tau_proto::ToolError),
    /// Cancelled terminal result.
    Cancelled(tau_proto::ToolCancelled),
}

impl ToolTerminalOutcome {
    /// Returns the final wire tool name for mutation by a local scoping
    /// adapter.
    #[must_use]
    pub fn tool_name_mut(&mut self) -> &mut tau_proto::ToolName {
        match self {
            Self::Result(result) => &mut result.tool_name,
            Self::Failure(error) => &mut error.tool_name,
            Self::Cancelled(cancelled) => &mut cancelled.tool_name,
        }
    }

    /// Converts the typed outcome into its transient peer report event.
    #[must_use]
    pub fn into_reported_event(self) -> tau_proto::Event {
        match self {
            Self::Result(result) => tau_proto::Event::ToolResultReported(result),
            Self::Failure(error) => tau_proto::Event::ToolErrorReported(error),
            Self::Cancelled(cancelled) => tau_proto::Event::ToolCancelledReported(cancelled),
        }
    }
}

impl From<tau_proto::ToolResult> for ToolTerminalOutcome {
    fn from(result: tau_proto::ToolResult) -> Self {
        Self::Result(result)
    }
}

impl From<tau_proto::ToolError> for ToolTerminalOutcome {
    fn from(error: tau_proto::ToolError) -> Self {
        Self::Failure(error)
    }
}

impl From<tau_proto::ToolCancelled> for ToolTerminalOutcome {
    fn from(cancelled: tau_proto::ToolCancelled) -> Self {
        Self::Cancelled(cancelled)
    }
}

impl TryFrom<tau_proto::Event> for ToolTerminalOutcome {
    type Error = tau_proto::Event;

    fn try_from(event: tau_proto::Event) -> Result<Self, Self::Error> {
        match event {
            tau_proto::Event::ToolResult(result) => Ok(Self::Result(result)),
            tau_proto::Event::ToolError(error) => Ok(Self::Failure(error)),
            tau_proto::Event::ToolCancelled(cancelled) => Ok(Self::Cancelled(cancelled)),
            event => Err(event),
        }
    }
}
