//! Calendar module for the standard PIM extension.
//!
//! This module establishes the model-visible split `calendar_*` tool surface
//! and actions owned by extensions.

mod actions;
mod config;
mod cursor;
mod google;
mod ics_feed;
mod runtime;
mod state;
mod tool;

pub use actions::calendar_action_schema;
pub(crate) use actions::calendar_action_schema_with_accounts;
pub use config::{
    CalendarAccountConfig, CalendarBackendConfig, CalendarExtensionConfig, CalendarPolicyConfig,
    CalendarReadPolicyConfig, CalendarSelectionConfig, CalendarWritePolicyConfig,
    DescriptionPolicy, PrivateEventsPolicy,
};
pub use google::GoogleBackend;
pub use ics_feed::IcsFeedBackend;
pub use runtime::RuntimeState;
pub(crate) use runtime::{initial_progress, is_tool_name};
pub use tool::{calendar_prompt_fragment, calendar_tool_spec, calendar_tool_specs};

/// Legacy envelope tool name for calendar commands.
pub const TOOL_NAME: &str = "calendar";

/// Prefix for model-visible split calendar command tools.
pub const TOOL_PREFIX: &str = "calendar_";
