//! Client-side state for actions provided by extensions.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use tau_proto::{ActionSchemaPublished, ExtensionInstanceId, ExtensionName};

use crate::locked;

#[derive(Clone, Debug, Eq, PartialEq)]
struct ActionOwner {
    extension_name: ExtensionName,
    instance_id: ExtensionInstanceId,
}

#[derive(Clone, Debug)]
struct RootBinding {
    owner: ActionOwner,
    schema: tau_actions::ActionSchema,
    description: String,
}

#[derive(Clone, Debug, Default)]
struct ActionCommandInner {
    schemas: BTreeMap<(String, u64), (ActionOwner, tau_actions::ActionSchema)>,
    roots: BTreeMap<String, RootBinding>,
}

/// Parsed dynamic action invocation ready to send to the harness.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ActionDispatch {
    /// Extension name from the schema snapshot selected by the UI.
    pub(crate) extension_name: ExtensionName,
    /// Extension instance id from the schema snapshot selected by the UI.
    pub(crate) instance_id: ExtensionInstanceId,
    /// Parsed extension action.
    pub(crate) parsed: tau_actions::ParsedAction,
}

/// Shared action schema snapshot used by the renderer, completer, and input
/// loop.
#[derive(Clone, Debug)]
pub(crate) struct ActionCommandState {
    builtin_roots: Arc<BTreeSet<String>>,
    inner: Arc<Mutex<ActionCommandInner>>,
}

impl ActionCommandState {
    /// Create an empty action-command state, filtering out any dynamic root
    /// that collides with the provided built-in command names.
    pub(crate) fn new(builtin_roots: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        let builtin_roots = builtin_roots
            .into_iter()
            .map(|root| root.as_ref().to_owned())
            .collect();
        Self {
            builtin_roots: Arc::new(builtin_roots),
            inner: Arc::new(Mutex::new(ActionCommandInner::default())),
        }
    }

    /// Apply one harness-stamped action schema publication.
    pub(crate) fn apply_schema_published(&self, published: &ActionSchemaPublished) {
        if let Err(error) = published.schema.validate() {
            tracing::warn!(
                target: "tau_cli::actions",
                extension = %published.extension_name,
                instance_id = published.instance_id.get(),
                %error,
                "ignoring invalid published action schema"
            );
            return;
        }

        let mut schema = published.schema.clone();
        schema
            .roots
            .retain(|root| !self.builtin_roots.contains(&root.name));
        let key = Self::owner_key(&published.extension_name, published.instance_id);
        let owner = ActionOwner {
            extension_name: published.extension_name.clone(),
            instance_id: published.instance_id,
        };
        let mut inner = locked(&self.inner);
        if schema.roots.is_empty() {
            inner.schemas.remove(&key);
        } else {
            inner.schemas.insert(key, (owner, schema));
        }
        Self::rebuild_roots(&mut inner);
    }

    /// Remove all action roots published by one extension instance.
    pub(crate) fn remove_extension(
        &self,
        extension_name: &ExtensionName,
        instance_id: ExtensionInstanceId,
    ) {
        let mut inner = locked(&self.inner);
        inner
            .schemas
            .remove(&Self::owner_key(extension_name, instance_id));
        Self::rebuild_roots(&mut inner);
    }

    /// Return true when the line begins with a currently known dynamic action
    /// root.
    pub(crate) fn is_known_action_line(&self, text: &str) -> bool {
        let Some(root) = text.split_whitespace().next() else {
            return false;
        };
        locked(&self.inner).roots.contains_key(root)
    }

    /// Parse a line if it belongs to a currently known dynamic action root.
    pub(crate) fn parse_line(
        &self,
        line: &str,
    ) -> Option<Result<ActionDispatch, tau_actions::ParseError>> {
        let root = line.split_whitespace().next()?;
        let binding = locked(&self.inner).roots.get(root).cloned()?;
        Some(
            binding
                .schema
                .parse_line(line)
                .map(|parsed| ActionDispatch {
                    extension_name: binding.owner.extension_name,
                    instance_id: binding.owner.instance_id,
                    parsed,
                }),
        )
    }

    /// Build completion entries for active dynamic action roots and nested
    /// action subcommands.
    pub(crate) fn dynamic_completions(
        &self,
    ) -> (
        Vec<tau_cli_term::CommandCompletion>,
        Vec<(tau_cli_term::CommandName, tau_cli_term::ArgCompleter)>,
    ) {
        let inner = locked(&self.inner);
        let commands = inner
            .roots
            .iter()
            .map(|(root, binding)| {
                tau_cli_term::CommandCompletion::new(root.clone(), binding.description.clone())
            })
            .collect();
        let completers = inner
            .roots
            .iter()
            .map(|(root, binding)| {
                (
                    tau_cli_term::CommandName::new(root.clone()),
                    action_arg_completer(binding.schema.roots[0].clone()),
                )
            })
            .collect();
        (commands, completers)
    }

    fn owner_key(
        extension_name: &ExtensionName,
        instance_id: ExtensionInstanceId,
    ) -> (String, u64) {
        (extension_name.to_string(), instance_id.get())
    }

    fn rebuild_roots(inner: &mut ActionCommandInner) {
        let mut roots = BTreeMap::new();
        for (owner, schema) in inner.schemas.values() {
            for root in &schema.roots {
                roots
                    .entry(root.name.clone())
                    .or_insert_with(|| RootBinding {
                        owner: owner.clone(),
                        schema: tau_actions::ActionSchema {
                            version: schema.version,
                            roots: vec![root.clone()],
                        },
                        description: format!("{} ({})", root.description, owner.extension_name),
                    });
            }
        }
        inner.roots = roots;
    }
}

fn action_arg_completer(root: tau_actions::ActionCommand) -> tau_cli_term::ArgCompleter {
    Arc::new(move |args| complete_action_args(&root, args))
}

fn complete_action_args(
    root: &tau_actions::ActionCommand,
    args: &[&str],
) -> Vec<tau_cli_term::CompletionItem> {
    let Some(partial) = args.last().copied() else {
        return complete_children(root, "");
    };
    let mut command = root;
    let mut index = 0;
    while index + 1 < args.len() && !command.children.is_empty() {
        let token = args[index];
        let Some(child) = command.children.iter().find(|child| child.name == token) else {
            return Vec::new();
        };
        command = child;
        index += 1;
    }

    if !command.children.is_empty() {
        return complete_children(command, partial);
    }

    let arg_index = args.len().saturating_sub(index + 1);
    let Some(arg) = command.args.get(arg_index) else {
        return Vec::new();
    };
    match &arg.kind {
        tau_actions::ActionArgKind::Enum { values } => complete_choices(values, partial),
        tau_actions::ActionArgKind::String
        | tau_actions::ActionArgKind::Integer
        | tau_actions::ActionArgKind::RestString => complete_choices(&arg.suggestions, partial),
    }
}

fn complete_children(
    command: &tau_actions::ActionCommand,
    partial: &str,
) -> Vec<tau_cli_term::CompletionItem> {
    ranked_items(
        command
            .children
            .iter()
            .map(|child| (child.name.as_str(), child.description.as_str())),
        partial,
    )
}

fn complete_choices(
    choices: &[tau_actions::ActionChoice],
    partial: &str,
) -> Vec<tau_cli_term::CompletionItem> {
    ranked_items(
        choices
            .iter()
            .map(|choice| (choice.value.as_str(), choice.description.as_str())),
        partial,
    )
}

fn ranked_items<'a>(
    values: impl IntoIterator<Item = (&'a str, &'a str)>,
    partial: &str,
) -> Vec<tau_cli_term::CompletionItem> {
    let needle = partial.to_lowercase();
    let mut prefix_matches = Vec::new();
    let mut substr_matches = Vec::new();
    for (value, description) in values {
        let lower = value.to_lowercase();
        let item = tau_cli_term::CompletionItem::new(value, description);
        if needle.is_empty() || lower.starts_with(&needle) {
            prefix_matches.push(item);
        } else if lower.contains(&needle) {
            substr_matches.push(item);
        }
    }
    prefix_matches.extend(substr_matches);
    prefix_matches
}

#[cfg(test)]
mod tests;
