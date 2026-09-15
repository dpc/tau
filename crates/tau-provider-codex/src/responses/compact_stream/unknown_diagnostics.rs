//! Bounded, payload-free discovery of native compact compatibility gaps.

use std::collections::BTreeSet;

/// Maximum distinct diagnostic kinds retained and logged by one attempt.
const MAX_KINDS: usize = 16;

/// Deduplicates compatibility observations without retaining provider payloads.
#[derive(Debug, Default)]
pub(super) struct UnknownDiagnostics {
    /// Only bounded sanitized labels and closed local classification strings.
    seen: BTreeSet<(&'static str, String, &'static str)>,
    /// Repeated or overflow observations, saturated rather than wrapped.
    suppressed: u64,
    /// Whether the fixed overflow observation has already been emitted.
    overflow_reported: bool,
}

impl UnknownDiagnostics {
    /// Logs one first-seen kind, with a single notice once capacity is
    /// exhausted.
    pub(super) fn observe(
        &mut self,
        prompt: &str,
        phase: super::CompactItemPhase,
        location: &'static str,
        kind: Option<&str>,
        disposition: &'static str,
    ) {
        let label = safe_label(kind);
        let key = (location, label.to_owned(), disposition);
        if self.seen.contains(&key) {
            self.suppressed = self.suppressed.saturating_add(1);
            return;
        }
        if self.seen.len() == MAX_KINDS {
            self.suppressed = self.suppressed.saturating_add(1);
            if !self.overflow_reported {
                self.overflow_reported = true;
                tracing::info!(
                    target: crate::LOG_TARGET,
                    agent_prompt_id = prompt, stage = "compact_shape_validation",
                    ?phase, disposition = "unknown_kind_limit", suppressed = self.suppressed,
                    "additional unknown compact kinds suppressed; inspect private received_response capture",
                );
            }
            return;
        }
        self.seen.insert(key);
        if disposition == "ignored_informational" {
            tracing::info!(
                target: crate::LOG_TARGET,
                agent_prompt_id = prompt, stage = "compact_shape_validation",
                ?phase, location, kind = label, disposition,
                "unrecognized native compact notification; inspect private received_response capture",
            );
        } else {
            tracing::warn!(
                target: crate::LOG_TARGET,
                agent_prompt_id = prompt, stage = "compact_shape_validation",
                ?phase, location, kind = label, disposition,
                "unsupported native compact material; inspect private received_response capture",
            );
        }
    }
}

/// Accepts short ASCII identifiers only; no arbitrary provider value is logged.
pub(super) fn safe_label(value: Option<&str>) -> &str {
    value
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 96
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        })
        .unwrap_or("<invalid-type>")
}

#[cfg(test)]
mod tests;
