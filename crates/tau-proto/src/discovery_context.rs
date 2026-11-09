//! Model-facing discovery snapshots and canonical delivery receipts.

#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};

/// A separately deduplicated piece of discovered guidance.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiscoveryContextItem {
    /// Skill name or canonical AGENTS.md path, qualified by kind.
    pub identity: String,
    /// Digest of the exact model-facing content.
    pub version: String,
    /// Advertisement or instruction text; never a loaded skill body.
    pub content: String,
}

/// Identity of one version currently applicable to an agent.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiscoveryContextVersion {
    /// Same qualified identity as the corresponding delivered item.
    pub identity: String,
    /// Exact content digest, not a file timestamp.
    pub version: String,
}

impl DiscoveryContextItem {
    /// Return the content-free identity used in a current-scope listing.
    #[must_use]
    pub fn reference(&self) -> DiscoveryContextVersion {
        DiscoveryContextVersion {
            identity: self.identity.clone(),
            version: self.version.clone(),
        }
    }
}

/// One passive discovery update, delivered within an existing agent turn.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiscoveryContextDelivery {
    /// Complete current applicability, including references to earlier text.
    pub scope: Vec<DiscoveryContextVersion>,
    /// New or changed items relative to their latest surviving delivery.
    pub items: Vec<DiscoveryContextItem>,
    /// Item identities that replace previously delivered content.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub updated: Vec<String>,
}

impl DiscoveryContextDelivery {
    /// Render initial and post-workdir user guidance identically.
    #[must_use]
    pub fn render(&self) -> String {
        self.render_envelopes().join("\n")
    }

    /// Render each top-level family separately for generic user text carriers.
    #[must_use]
    pub fn render_envelopes(&self) -> Vec<String> {
        use std::fmt::Write as _;

        let mut envelopes = Vec::new();
        let mut skills = String::from("\n");
        for item in &self.items {
            let updated = self.updated.contains(&item.identity);
            if let Some(name) = item.identity.strip_prefix("skill:") {
                let marker = if updated { " (updated)" } else { "" };
                let _ = writeln!(skills, "* `{name}`{marker} - {}", item.content);
            }
        }
        if skills.len() > 1 {
            envelopes.push(crate::SKILLS_PAYLOAD_ENVELOPE.render(&skills));
        }
        for item in &self.items {
            if let Some(path) = item.identity.strip_prefix("agents:") {
                let mut attributes = vec![("path", crate::visible_escape_metadata(path))];
                if self.updated.contains(&item.identity) {
                    attributes.push(("updated", "true".to_owned()));
                }
                let mut body = format!("\n{}", item.content);
                if !body.ends_with('\n') {
                    body.push('\n');
                }
                envelopes.push(
                    crate::AGENTS_MD_PAYLOAD_ENVELOPE
                        .render_attributed(&attributes, &body)
                        .expect("discovery attributes follow the shared registry"),
                );
            }
        }
        envelopes
    }
}
