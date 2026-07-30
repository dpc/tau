use std::fmt;

use serde::{Deserialize, Serialize, de as path_serde_de};

use crate::{ToolGroupName, ToolName};

/// A configured prefix for structural tool identifiers.
///
/// Prefixes consist of non-empty ASCII-alphanumeric components separated by
/// single underscores. Composition is additive and never attempts to detect an
/// already-prefixed local name.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize)]
#[serde(transparent)]
pub struct ToolNamePrefix(String);

impl ToolNamePrefix {
    /// Maximum useful standalone prefix length.
    pub const MAX_LEN: usize = ToolName::MAX_LEN - 2;

    /// Parse and validate a configured tool prefix.
    ///
    /// # Errors
    ///
    /// Returns a diagnostic when the prefix is empty, too long, contains a
    /// non-ASCII-alphanumeric component, or has empty underscore-delimited
    /// components.
    pub fn parse(value: impl Into<String>) -> Result<Self, InvalidToolNamePrefix> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= Self::MAX_LEN
            && value.split('_').all(|component| {
                !component.is_empty() && component.bytes().all(|b| b.is_ascii_alphanumeric())
            });
        if valid {
            Ok(Self(value))
        } else {
            Err(InvalidToolNamePrefix { value })
        }
    }

    /// Borrow the validated prefix.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Compose a local tool or model-visible alias into its final wire name.
    ///
    /// # Errors
    ///
    /// Returns an error if the composed identifier exceeds the tool-name limit.
    pub fn compose_tool_name(
        &self,
        local: &ToolName,
    ) -> Result<ToolName, ToolNameCompositionError> {
        ToolName::try_new(format!("{}_{}", self.0, local.as_str())).ok_or_else(|| {
            ToolNameCompositionError {
                prefix: self.clone(),
                local: local.to_string(),
                target: ToolNameTarget::Tool,
            }
        })
    }

    /// Compose a local tool group into its final wire name.
    ///
    /// # Errors
    ///
    /// Returns an error if the composed identifier exceeds the group-name
    /// limit.
    pub fn compose_group_name(
        &self,
        local: &ToolGroupName,
    ) -> Result<ToolGroupName, ToolNameCompositionError> {
        ToolGroupName::try_new(format!("{}_{}", self.0, local.as_str())).ok_or_else(|| {
            ToolNameCompositionError {
                prefix: self.clone(),
                local: local.to_string(),
                target: ToolNameTarget::Group,
            }
        })
    }

    /// Returns whether a final tool name is inside this prefix's exact
    /// component envelope.
    #[must_use]
    pub fn contains_tool_name(&self, name: &ToolName) -> bool {
        name.as_str()
            .strip_prefix(self.as_str())
            .is_some_and(|suffix| suffix.starts_with('_') && suffix.len() > 1)
    }

    /// Returns whether a final group name is inside this prefix's exact
    /// component envelope.
    #[must_use]
    pub fn contains_group_name(&self, name: &ToolGroupName) -> bool {
        name.as_str()
            .strip_prefix(self.as_str())
            .is_some_and(|suffix| suffix.starts_with('_') && suffix.len() > 1)
    }
}

impl fmt::Display for ToolNamePrefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ToolNamePrefix {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(path_serde_de::Error::custom)
    }
}

/// Validation error for a configured [`ToolNamePrefix`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvalidToolNamePrefix {
    /// Rejected value.
    value: String,
}

impl fmt::Display for InvalidToolNamePrefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid tool prefix {:?}: expected ASCII alphanumeric components separated by single underscores (maximum {} bytes)",
            self.value,
            ToolNamePrefix::MAX_LEN
        )
    }
}

impl std::error::Error for InvalidToolNamePrefix {}

/// Structural identifier kind that failed prefix composition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolNameTarget {
    /// Tool internal name or model-visible alias.
    Tool,
    /// Tool group name.
    Group,
}

/// Error returned when a valid prefix and valid local identifier do not fit in
/// the final protocol identifier limit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolNameCompositionError {
    /// Configured prefix.
    pub prefix: ToolNamePrefix,
    /// Local identifier that was being composed.
    pub local: String,
    /// Structural identifier kind.
    pub target: ToolNameTarget,
}

impl fmt::Display for ToolNameCompositionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "tool prefix `{}` and local {} name `{}` exceed the final identifier limit",
            self.prefix,
            match self.target {
                ToolNameTarget::Tool => "tool",
                ToolNameTarget::Group => "group",
            },
            self.local
        )
    }
}

impl std::error::Error for ToolNameCompositionError {}
