//! Tool and action registration, routing, and lifecycle ownership.

use super::*;

/// Harness-owned tool and action routing state.
pub(crate) struct ToolRoutingState {
    /// Tool-name to provider registry.
    pub(crate) registry: ToolRegistry,
    /// UI-action to extension registry.
    pub(crate) action_registry: ActionRegistry,
    /// In-process tool handlers.
    pub(crate) internal_tool_handlers: InternalToolHandlers,
    /// Live and completed tool lifecycle state.
    pub(crate) tool_runtime: ToolRuntimeState,
}
