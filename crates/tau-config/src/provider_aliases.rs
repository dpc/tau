//! Startup-only provider alias graph validation and memoized selection.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use tau_proto::ProviderName;

use crate::settings::SettingsError;

/// A legacy single target or a nonempty set of alternative alias targets.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub(crate) enum ProviderAliasTargets {
    /// One provider or another alias.
    Single(ProviderName),
    /// Alternatives, replaced as a whole by later configuration layers.
    Multiple(Vec<ProviderName>),
}

impl ProviderAliasTargets {
    /// Borrows targets uniformly, preserving scalar identity-edge semantics.
    fn targets(&self) -> &[ProviderName] {
        match self {
            Self::Single(target) => std::slice::from_ref(target),
            Self::Multiple(targets) => targets,
        }
    }
}

/// Validates all branches, then selects each alias once for the complete
/// launch.
pub(crate) fn select(
    aliases: &BTreeMap<ProviderName, ProviderAliasTargets>,
    mut score: impl FnMut(&ProviderName) -> Option<u16>,
    mut draw: impl FnMut(usize) -> usize,
) -> Result<BTreeMap<ProviderName, ProviderName>, SettingsError> {
    let mut validated = BTreeSet::new();
    for name in aliases.keys() {
        validate(aliases, name, &mut Vec::new(), &mut validated)?;
    }
    let mut selected = BTreeMap::new();
    for name in aliases.keys() {
        resolve(aliases, name, &mut selected, &mut score, &mut draw);
    }
    Ok(selected)
}

/// Walks even unused alternatives so random choice cannot conceal bad config.
fn validate(
    aliases: &BTreeMap<ProviderName, ProviderAliasTargets>,
    name: &ProviderName,
    path: &mut Vec<ProviderName>,
    validated: &mut BTreeSet<ProviderName>,
) -> Result<(), SettingsError> {
    if validated.contains(name) {
        return Ok(());
    }
    let Some(targets) = aliases.get(name) else {
        return Ok(());
    };
    if let Some(position) = path.iter().position(|entry| entry == name) {
        let mut cycle = path[position..].to_vec();
        cycle.push(name.clone());
        return Err(SettingsError::ProviderAliasCycle { path: cycle });
    }
    if targets.targets().is_empty() {
        return Err(SettingsError::Config(config::ConfigError::Message(
            format!("provider alias {name} must have at least one target"),
        )));
    }
    path.push(name.clone());
    for target in targets.targets() {
        if target != name {
            validate(aliases, target, path, validated)?;
        }
    }
    path.pop();
    validated.insert(name.clone());
    Ok(())
}

/// Resolves children first and deduplicates concrete choices before ranking.
fn resolve(
    aliases: &BTreeMap<ProviderName, ProviderAliasTargets>,
    name: &ProviderName,
    selected: &mut BTreeMap<ProviderName, ProviderName>,
    score: &mut impl FnMut(&ProviderName) -> Option<u16>,
    draw: &mut impl FnMut(usize) -> usize,
) -> ProviderName {
    if let Some(target) = selected.get(name) {
        return target.clone();
    }
    let Some(targets) = aliases.get(name) else {
        return name.clone();
    };
    let candidates: BTreeSet<_> = targets
        .targets()
        .iter()
        .map(|target| {
            if target == name {
                name.clone()
            } else {
                resolve(aliases, target, selected, score, draw)
            }
        })
        .collect();
    let scores: Vec<_> = candidates
        .into_iter()
        .map(|name| {
            let remaining = score(&name);
            (name, remaining)
        })
        .collect();
    let best = scores.iter().filter_map(|(_, score)| *score).max();
    let choices: Vec<_> = scores
        .into_iter()
        .filter(|(_, score)| *score == best)
        .map(|(name, _)| name)
        .collect();
    let chosen = choices[draw(choices.len())].clone();
    selected.insert(name.clone(), chosen.clone());
    chosen
}

#[cfg(test)]
mod tests;
