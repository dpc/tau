//! Snapshot records for skills and AGENTS.md files announced by extensions
//! during session init.

use std::borrow::Cow;
use std::path::PathBuf;
use std::time::SystemTime;

/// Where a discovered skill's Markdown source lives.
#[derive(Clone)]
pub(crate) enum DiscoveredSkillSource {
    /// An extension-announced skill backed by an on-disk Markdown file.
    File(PathBuf),
    /// A Tau built-in skill embedded into the harness binary.
    BuiltIn { content: Cow<'static, str> },
}

impl DiscoveredSkillSource {
    #[cfg(test)]
    pub(crate) fn file_path(&self) -> Option<&std::path::Path> {
        match self {
            Self::File(path) => Some(path.as_path()),
            Self::BuiltIn { .. } => None,
        }
    }

    pub(crate) fn label(&self) -> String {
        match self {
            Self::File(path) => path.display().to_string(),
            Self::BuiltIn { .. } => "built-in skill".to_owned(),
        }
    }
}

/// A skill discovered by an extension or bundled into Tau.
#[derive(Clone)]
pub(crate) struct DiscoveredSkill {
    pub(crate) source_id: tau_proto::ConnectionId,
    pub(crate) description: String,
    pub(crate) source: DiscoveredSkillSource,
    pub(crate) add_to_prompt: bool,
    pub(crate) user_invocable: bool,
    pub(crate) disable_model_invocation: bool,
    /// Optional argument synopsis shown by interactive completion.
    pub(crate) argument_hint: Option<String>,
    pub(crate) modified: Option<SystemTime>,
}

/// One AGENTS.md file discovered by an extension.
#[derive(Clone)]
pub(crate) struct DiscoveredAgentsFile {
    pub(crate) source_id: tau_proto::ConnectionId,
    pub(crate) file_path: PathBuf,
    pub(crate) content: String,
}
