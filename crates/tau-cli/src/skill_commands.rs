//! Client-side state for `:skill` command completion.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::locked;

/// Shared skill snapshot used by the renderer and prompt completer.
#[derive(Clone, Debug, Default)]
pub(crate) struct SkillCommandState {
    inner: Arc<Mutex<BTreeMap<String, SkillCompletion>>>,
}

#[derive(Clone, Debug)]
struct SkillCompletion {
    description: String,
    argument_hint: Option<String>,
    source_label: String,
}

impl SkillCommandState {
    /// Create an empty skill-command state.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Replace completion state from one canonical complete session snapshot.
    pub(crate) fn apply_session_snapshot(
        &self,
        snapshot: &tau_proto::HarnessSessionSkillsAvailable,
    ) {
        let mut inner = locked(&self.inner);
        inner.clear();
        for skill in &snapshot.skills {
            if !skill.user_invocable {
                continue;
            }
            let source_label = match &skill.source {
                tau_proto::DiscoveryEffectiveSkillSource::File { path } => {
                    path.display().to_string()
                }
                tau_proto::DiscoveryEffectiveSkillSource::BuiltIn => "built-in skill".to_owned(),
            };
            inner.insert(
                skill.name.to_string(),
                SkillCompletion {
                    description: skill.description.clone(),
                    argument_hint: skill.argument_hint.clone(),
                    source_label,
                },
            );
        }
    }

    /// Build the `:skill` argument completer for the current snapshot.
    pub(crate) fn arg_completer(&self) -> tau_cli_term::ArgCompleter {
        let state = self.clone();
        Arc::new(move |args| state.complete_args(args))
    }

    fn complete_args(&self, args: &[&str]) -> Vec<tau_cli_term::CompletionItem> {
        if args.len() != 1 {
            return Vec::new();
        }
        let needle = args[0].to_lowercase();
        let mut prefix_matches = Vec::new();
        let mut substring_matches = Vec::new();
        for (name, skill) in locked(&self.inner).iter() {
            let lower_name = name.to_lowercase();
            let item = tau_cli_term::CompletionItem::new(name, skill.menu_description());
            if needle.is_empty() || lower_name.starts_with(&needle) {
                prefix_matches.push(item);
            } else if lower_name.contains(&needle) {
                substring_matches.push(item);
            }
        }
        prefix_matches.extend(substring_matches);
        prefix_matches
    }
}

impl SkillCompletion {
    fn menu_description(&self) -> String {
        let mut description = self.description.clone();
        if let Some(hint) = self
            .argument_hint
            .as_deref()
            .filter(|hint| !hint.is_empty())
        {
            description.push_str(" — ");
            description.push_str(hint);
        }
        description.push_str(" (");
        description.push_str(&self.source_label);
        description.push(')');
        description
    }
}

#[cfg(test)]
mod tests;
