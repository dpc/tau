//! Shared, sampled role policy for context files; this is not access control.

use serde::{Deserialize, Serialize};

/// Role and group selectors sampled when a context file is discovered.
///
/// Names are exact and case-sensitive. Unknown names are inert. Allow
/// dimensions form a union; exclusions are applied afterwards and always win.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ContextVisibility {
    /// Allowed roles; absence differs from an explicitly empty allowlist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub only_roles: Option<Vec<String>>,
    /// Allowed configured groups, unioned with allowed roles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub only_role_groups: Option<Vec<String>>,
    /// Roles removed after evaluating both allowlists.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub except_roles: Vec<String>,
    /// Configured groups removed after evaluating both allowlists.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub except_role_groups: Vec<String>,
}

impl ContextVisibility {
    /// Count selector text bytes for the discovery snapshot admission budget.
    #[must_use]
    pub fn selector_bytes(&self) -> usize {
        self.only_roles
            .iter()
            .flatten()
            .chain(self.only_role_groups.iter().flatten())
            .chain(self.except_roles.iter())
            .chain(self.except_role_groups.iter())
            .fold(0usize, |bytes, selector| {
                bytes.saturating_add(selector.len())
            })
    }

    /// Test a finalized role and its configured group (or role-name fallback).
    #[must_use]
    pub fn allows(&self, role: &str, group: &str) -> bool {
        let contains = |names: &[String], name: &str| names.iter().any(|item| item == name);
        let allowed = (self.only_roles.is_none() && self.only_role_groups.is_none())
            || self
                .only_roles
                .as_ref()
                .is_some_and(|names| contains(names, role))
            || self
                .only_role_groups
                .as_ref()
                .is_some_and(|names| contains(names, group));
        allowed && !contains(&self.except_roles, role) && !contains(&self.except_role_groups, group)
    }
}
