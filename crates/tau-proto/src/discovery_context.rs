//! Model-facing discovery snapshots and canonical delivery receipts.

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
    /// Versions not already represented in surviving canonical context.
    pub items: Vec<DiscoveryContextItem>,
}

impl DiscoveryContextDelivery {
    /// Render initial and post-workdir user guidance identically.
    #[must_use]
    pub fn render(&self) -> String {
        use std::fmt::Write as _;

        let mut body = String::from(
            "Discovery context\n\n\
             This is the current applicable skill catalog and AGENTS.md guidance. \
             Earlier versions remain history; only the versions listed below are \
             currently applicable. References without new text refer to the same \
             identity and version already delivered in this context. \
             AGENTS.md instructions are ordered broadest to most specific; \
             more specific files usually override broader ones.\n\n\
             Current scope:\n",
        );
        for item in &self.scope {
            let _ = writeln!(body, "- {} (version {})", item.identity, item.version);
        }
        if self.scope.is_empty() {
            body.push_str("- No advertised skills or AGENTS.md instructions.\n");
        }
        for item in &self.items {
            let _ = write!(
                body,
                "\n## {} (version {})\n\n{}\n",
                item.identity, item.version, item.content
            );
        }
        format!(
            "<discovery>\n{}</discovery>",
            crate::DISCOVERY_PAYLOAD_ENVELOPE.escape_body(&body)
        )
    }
}
