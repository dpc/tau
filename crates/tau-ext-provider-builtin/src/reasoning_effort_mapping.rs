//! Provider-profile reasoning-effort mapping configuration.

use serde::{Deserialize, Serialize};

/// Exact portable-intensity cut points for one provider/model route.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReasoningEffortMapping {
    /// Strictly increasing native levels and portable cut points.
    pub mapping: Vec<tau_proto::ReasoningEffortBand>,
}

impl ReasoningEffortMapping {
    /// Construct the standard cut points for an ordered native-level set.
    #[must_use]
    pub fn standard(levels: impl IntoIterator<Item = tau_proto::NativeReasoningEffort>) -> Self {
        let capability = tau_proto::ReasoningEffortCapability::mapped(levels);
        let tau_proto::ReasoningEffortControl::Mapped { mapping } = capability.control else {
            return Self::default();
        };
        Self { mapping }
    }

    /// Return whether the configured mapping is structurally valid.
    ///
    /// An empty mapping is valid profile syntax for routes that use it to
    /// disable reasoning-effort control.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.mapping.is_empty() || self.capability().is_valid()
    }

    /// Return whether this mapping contains no selectable native level.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.mapping.is_empty()
    }

    /// Return the number of selectable native levels.
    #[must_use]
    pub fn len(&self) -> usize {
        self.mapping.len()
    }

    /// Publish this configured mapping as the shared model capability.
    #[must_use]
    pub fn capability(&self) -> tau_proto::ReasoningEffortCapability {
        if self.mapping.is_empty() {
            return tau_proto::ReasoningEffortCapability::default();
        }
        tau_proto::ReasoningEffortCapability {
            control: tau_proto::ReasoningEffortControl::Mapped {
                mapping: self.mapping.clone(),
            },
            provider_default: None,
        }
    }
}
