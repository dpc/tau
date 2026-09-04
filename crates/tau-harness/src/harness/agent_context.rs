//! Per-agent context contributions published by extensions.

use std::collections::BTreeMap;

/// One extension contribution for one agent context key.
#[derive(Clone, Debug)]
struct AgentContextContribution {
    /// Human-readable extension name shown to prompt templates.
    extension_name: String,
    /// JSON-compatible value contributed by the extension.
    value: tau_proto::AgentContextValue,
}

/// Store for session-scoped JSON context contributions keyed by agent.
#[derive(Clone, Debug, Default)]
pub(crate) struct AgentContextStore {
    /// `agent_id -> context key -> contributor connection -> contribution`.
    by_agent: BTreeMap<
        tau_proto::AgentId,
        BTreeMap<
            tau_proto::AgentContextKey,
            BTreeMap<tau_proto::ConnectionId, AgentContextContribution>,
        >,
    >,
}

impl AgentContextStore {
    /// Store or replace one contributor's value for an agent context key.
    pub(crate) fn publish(
        &mut self,
        agent_id: tau_proto::AgentId,
        key: tau_proto::AgentContextKey,
        contributor: tau_proto::ConnectionId,
        extension_name: String,
        value: tau_proto::AgentContextValue,
    ) {
        self.by_agent
            .entry(agent_id)
            .or_default()
            .entry(key)
            .or_default()
            .insert(
                contributor,
                AgentContextContribution {
                    extension_name,
                    value,
                },
            );
    }

    /// Remove every contribution published by one extension connection.
    pub(crate) fn remove_contributor(&mut self, contributor: &tau_proto::ConnectionId) {
        self.by_agent.retain(|_, keys| {
            keys.retain(|_, contributions| {
                contributions.remove(contributor);
                !contributions.is_empty()
            });
            !keys.is_empty()
        });
    }

    /// Removes all context contributions for isolated test setup.
    #[cfg(test)]
    pub(crate) fn clear(&mut self) {
        self.by_agent.clear();
    }

    /// Return the Handlebars-visible `agent_context` object for one agent.
    #[cfg(test)]
    pub(crate) fn template_value(
        &self,
        agent_id: Option<&tau_proto::AgentId>,
    ) -> serde_json::Value {
        self.template_value_filtered(agent_id, |_, _| true)
    }

    /// Return the Handlebars-visible context after applying one prompt-specific
    /// contributor filter.
    pub(crate) fn template_value_filtered(
        &self,
        agent_id: Option<&tau_proto::AgentId>,
        mut include: impl FnMut(&tau_proto::AgentContextKey, &tau_proto::ConnectionId) -> bool,
    ) -> serde_json::Value {
        let mut object = serde_json::Map::new();
        let Some(agent_id) = agent_id else {
            return serde_json::Value::Object(object);
        };
        let Some(keys) = self.by_agent.get(agent_id) else {
            return serde_json::Value::Object(object);
        };
        for (key, contributions) in keys {
            let mut wrappers: Vec<_> = contributions
                .iter()
                .filter(|(connection_id, _)| include(key, connection_id))
                .map(|(connection_id, contribution)| {
                    (
                        contribution.extension_name.clone(),
                        connection_id.clone(),
                        serde_json::json!({
                            "extension_name": contribution.extension_name,
                            "value": contribution.value.0,
                        }),
                    )
                })
                .collect();
            wrappers.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
            object.insert(
                key.to_string(),
                serde_json::Value::Array(wrappers.into_iter().map(|(_, _, value)| value).collect()),
            );
        }
        serde_json::Value::Object(object)
    }
}
