//! Effective startup configuration and runtime policy selections.

use super::*;

/// Effective harness configuration retained for runtime decisions.
pub(crate) struct HarnessConfigState {
    /// Provider settings captured before instance spawn.
    pub(crate) provider_settings_snapshots: BTreeMap<String, BTreeMap<String, Vec<u8>>>,
    /// Complete accepted startup settings.
    pub(crate) accepted_harness_settings: tau_config::settings::HarnessSettings,
    /// Available agent roles.
    pub(crate) available_roles: HashMap<String, tau_config::settings::AgentRole>,
    /// Reasons configured roles are unavailable.
    pub(crate) disabled_role_reasons: HashMap<String, DisabledRoleReason>,
    /// Ordered role groups visible to clients.
    pub(crate) available_role_groups: Vec<tau_proto::HarnessRoleGroup>,
    /// Receiver-capable roles in configured order.
    pub(crate) inter_session_receivers: Vec<crate::model::InterSessionReceiverRole>,
    /// Reusable prompts from startup settings.
    pub(crate) custom_prompts: Vec<tau_proto::HarnessCustomPrompt>,
    /// Runtime role overrides.
    pub(crate) role_overrides: HashMap<String, tau_config::settings::AgentRole>,
    /// Declarative tool tag policy.
    pub(crate) tool_policy: tau_config::settings::ToolPolicy,
    /// Currently selected role.
    pub(crate) selected_role: String,
    /// Model resolved for the selected role.
    pub(crate) selected_model: Option<ModelId>,
    /// Template used to mint agent identifiers.
    pub(crate) agent_id_template: String,
    /// Template used to display newly created agents.
    pub(crate) agent_display_name_template: Option<String>,
}
