//! Harness daemon: manages extensions, routing, session state, and
//! serves socket clients.
//!
//! Each connection has a reader thread and a writer thread.  All
//! reader threads feed one shared `mpsc::channel`.  The harness event
//! loop blocks on `rx.recv()` and dispatches instantly.  The bus
//! delivers outgoing events by sending to per-connection writer
//! channels (non-blocking).  Writer threads drain their channel and
//! write to the stream; on channel close they run the shutdown
//! sequence for that connection.
//!
//! Event publication/replay and durable session behavior are specified by
//! `SPEC-tau-harness-event-processing` and `SPEC-tau-harness-session-state`.

pub mod runtime_dir;

pub(crate) const INTERNAL_MARKER: &str = "[tau-internal]";

mod agent;
mod daemon;
mod debug_log;
mod dedup;
mod dirs;
mod discovery;
mod error;
mod event;
mod event_log;
mod extension;
mod format;
mod harness;
pub mod internal_tools;
mod model;
mod prompt;
mod secrets;
mod session_cleanup;
mod settings;
mod tool_turn;
mod turn;
pub mod version;

pub fn dump_initial_prompt(
    out_path: &std::path::Path,
    user_message: &str,
) -> Result<(), HarnessError> {
    harness::Harness::dump_initial_prompt(out_path, user_message)
}

pub use tau_core::{AgentEntry, AgentTree, SessionMeta, list_session_metas, session_is_locked};
pub use tau_proto::AgentId;

pub(crate) fn parse_agent_id(value: impl AsRef<str>) -> AgentId {
    AgentId::parse(value.as_ref()).expect("harness stores only valid agent ids")
}

pub use crate::daemon::{
    EPHEMERAL_ENV, EmbeddedOptions, InteractionOutcome, ServeOptions, SessionLaunchStatus,
    get_daemon_rendered_system_prompt, get_daemon_rendered_tool_definitions, run_component,
    run_component_with_internal_tools,
    run_component_with_internal_tools_and_extension_cli_overrides,
    run_component_with_internal_tools_and_initial_ui_stdio, run_daemon, run_daemon_with_config,
    run_embedded_message, run_embedded_message_with_options, run_embedded_message_with_trace,
    run_harness_daemon, run_harness_daemon_with_internal_tools, send_daemon_message,
    send_daemon_message_with_trace,
};
#[cfg(any(test, feature = "echo-agent"))]
pub use crate::daemon::{run_daemon_with_echo, run_embedded_message_with_echo};
pub use crate::error::HarnessError;
pub use crate::extension::{harness_log_path, session_logs_dir};
pub use crate::format::{format_extension_event, format_tool_progress};
pub use crate::harness::{AgentToolCall, Harness};
pub use crate::internal_tools::{InternalToolHandler, InternalToolHandlers, InternalToolHost};
pub use crate::settings::{
    EXTENSION_CLI_OVERRIDES_ENV, HARNESS_CONFIG_CLI_OVERRIDES_ENV, ROLE_CLI_OVERRIDES_ENV,
    STARTUP_ROLE_ENV, builtin_extensions, default_config, validate_cli_overrides,
    validate_extension_environment_and_cli_overrides,
};
