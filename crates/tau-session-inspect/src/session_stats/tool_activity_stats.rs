use serde::Serialize;
use tau_proto::ToolName;

/// Tool-call and terminal counts for one tool name.
#[derive(Clone, Debug, Serialize)]
pub struct ToolActivityStats {
    /// Stable model-visible tool name.
    pub tool: ToolName,
    /// Calls emitted by canonical provider responses.
    pub calls: u64,
    /// Successful terminals.
    pub results: u64,
    /// Failed terminals.
    pub errors: u64,
    /// Cancelled terminals.
    pub cancellations: u64,
}

impl ToolActivityStats {
    pub(super) fn new(tool: ToolName) -> Self {
        Self {
            tool,
            calls: 0,
            results: 0,
            errors: 0,
            cancellations: 0,
        }
    }
}
