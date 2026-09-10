//! Shared ranked selection for hosted policy candidates and ordinary backings.

use std::collections::{BTreeMap, HashSet};

use tau_proto::{ToolName, ToolSpec};

/// One eligible implementation with a deterministic policy-owned ordering.
pub(super) struct RankedBacking<'a, T> {
    /// Stable candidate identity used to break equal configured priorities.
    pub(super) name: &'a str,
    /// Lower values are preferred, matching existing logical web policy.
    pub(super) priority: i64,
    /// Hosted or registered implementation selected by the caller.
    pub(super) backing: T,
}

/// Selects the first eligible implementation without executing or retrying it.
pub(super) fn select_backing<T>(
    mut candidates: Vec<RankedBacking<'_, T>>,
    eligible: impl Fn(&T) -> bool,
) -> Option<T> {
    candidates.sort_by(|left, right| {
        left.priority
            .cmp(&right.priority)
            .then_with(|| left.name.cmp(right.name))
    });
    candidates
        .into_iter()
        .find(|candidate| eligible(&candidate.backing))
        .map(|candidate| candidate.backing)
}

/// Automatically prefers a provider-specific implementation of each alias.
///
/// Scope and role authorization must already have been checked. Existing web
/// policy owns its aliases separately. Ambiguous declarations within either
/// tier remain errors even when the other tier has a preferred implementation.
pub(super) fn select_ordinary_backings(
    specs: &mut Vec<ToolSpec>,
    managed_aliases: &[&str],
) -> Result<(), ToolName> {
    let mut groups = BTreeMap::<String, Vec<&ToolSpec>>::new();
    for spec in specs.iter() {
        let alias = spec.model_visible_name.as_ref().unwrap_or(&spec.name);
        if !managed_aliases.contains(&alias.as_str()) {
            groups.entry(alias.to_string()).or_default().push(spec);
        }
    }
    let mut retained = HashSet::new();
    for (alias, backings) in groups {
        let scoped = backings
            .iter()
            .filter(|spec| spec.provider_scope.is_some())
            .count();
        if 1 < scoped || backings.len() - scoped > 1 {
            return Err(ToolName::new(alias));
        }
        let candidates = backings
            .into_iter()
            .map(|spec| RankedBacking {
                name: spec.name.as_str(),
                priority: if spec.provider_scope.is_some() {
                    10
                } else {
                    20
                },
                backing: spec.name.clone(),
            })
            .collect();
        if let Some(name) = select_backing(candidates, |_| true) {
            retained.insert(name);
        }
    }
    specs.retain(|spec| {
        let alias = spec.model_visible_name.as_ref().unwrap_or(&spec.name);
        managed_aliases.contains(&alias.as_str()) || retained.contains(&spec.name)
    });
    Ok(())
}
