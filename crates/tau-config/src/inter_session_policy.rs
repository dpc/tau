//! Session-level inbound receiver and outbound access policy.

use std::path::Path;

use globset::{GlobBuilder, GlobMatcher};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Session-wide policy for bare-message receiving and remote-session access.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct InterSessionPolicy {
    /// Optional bare-session receiver. Omission disables bare-session
    /// addressing.
    pub receiver: Option<InterSessionReceiver>,
    /// Optional advisory text attached to every outgoing cross-session message.
    pub outgoing_notice: Option<tau_proto::InterSessionNotice>,
    /// Optional recipient-local advisory text attached to every incoming
    /// cross-session message.
    pub incoming_notice: Option<tau_proto::InterSessionNotice>,
    /// Optional allowlist evaluated before the denylist.
    ///
    /// `None` permits every project root, while an explicit empty list permits
    /// none.
    pub allow_project_roots: Option<Vec<ProjectRootGlob>>,
    /// Optional denylist whose matches always veto access.
    pub deny_project_roots: Option<Vec<ProjectRootGlob>>,
}

/// Single role authorized to receive bare inter-session messages.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InterSessionReceiver {
    /// Effective enabled role used for existing and auto-started recipients.
    pub role: String,
    /// Whether Tau may start the role when no eligible live instance exists.
    #[serde(default = "default_receiver_auto_start")]
    pub auto_start: bool,
}

const fn default_receiver_auto_start() -> bool {
    true
}

impl InterSessionPolicy {
    /// Returns whether one canonical absolute project root is accessible.
    #[must_use]
    pub fn allows(&self, project_root: &Path) -> bool {
        let allowed = self
            .allow_project_roots
            .as_ref()
            .is_none_or(|patterns| patterns.iter().any(|pattern| pattern.matches(project_root)));
        allowed
            && !self.deny_project_roots.as_ref().is_some_and(|patterns| {
                patterns.iter().any(|pattern| pattern.matches(project_root))
            })
    }
}

/// One validated glob matched against a canonical absolute project root.
#[derive(Clone, Debug)]
pub struct ProjectRootGlob {
    /// Authored absolute glob retained for serialization and diagnostics.
    pattern: String,
    /// Compiled path matcher with separators treated literally.
    matcher: GlobMatcher,
}

impl ProjectRootGlob {
    /// Compiles one absolute globset-grammar project-root pattern.
    pub fn new(pattern: String) -> Result<Self, String> {
        if !pattern.starts_with('/') {
            return Err(
                "inter-session project-root globs must be absolute and start with `/`".to_owned(),
            );
        }
        let matcher = GlobBuilder::new(&pattern)
            .literal_separator(true)
            .backslash_escape(true)
            .build()
            .map_err(|error| {
                format!("invalid inter-session project-root glob `{pattern}`: {error}")
            })?
            .compile_matcher();
        Ok(Self { pattern, matcher })
    }

    /// Returns whether this glob matches the supplied canonical project root.
    #[must_use]
    pub fn matches(&self, project_root: &Path) -> bool {
        self.matcher.is_match(project_root)
    }

    /// Returns the authored glob.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.pattern
    }
}

impl PartialEq for ProjectRootGlob {
    fn eq(&self, other: &Self) -> bool {
        self.pattern == other.pattern
    }
}

impl Eq for ProjectRootGlob {}

impl Serialize for ProjectRootGlob {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.pattern)
    }
}

impl<'de> Deserialize<'de> for ProjectRootGlob {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests;
