//! Exact logical prompt lifecycle support for provider-builtin retry
//! acceptance.

use std::collections::BTreeMap;

use tau_proto::Event;

/// Tracks logical prompt lifecycle rather than HTTP attempt count.
#[derive(Default)]
pub(super) struct Lifecycle {
    /// Exact counters keyed by harness-owned logical prompt identity.
    prompts: BTreeMap<tau_proto::AgentPromptId, LifecycleCounts>,
}

/// Exact canonical and transient lifecycle counts for one logical prompt.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct LifecycleCounts {
    /// Canonical prompt creation facts.
    created: usize,
    /// Canonical provider submission facts.
    submitted: usize,
    /// Canonical provider terminal facts.
    finished: usize,
    /// Transient typed retry-state updates.
    retries: usize,
}

impl Lifecycle {
    /// Adds one event to the exact logical lifecycle counters.
    pub(super) fn record(&mut self, event: &Event) {
        match event {
            Event::AgentPromptCreated(value) => {
                self.counts_mut(&value.agent_prompt_id).created += 1;
            }
            Event::ProviderPromptSubmitted(value) => {
                self.counts_mut(&value.agent_prompt_id).submitted += 1;
            }
            Event::ProviderResponseFinished(value) => {
                self.counts_mut(&value.agent_prompt_id).finished += 1;
            }
            Event::ProviderResponseUpdated(value)
                if value
                    .status
                    .as_ref()
                    .is_some_and(|status| status.retry.is_some()) =>
            {
                self.counts_mut(&value.agent_prompt_id).retries += 1;
            }
            _ => {}
        }
    }

    /// Requires the parked prompt to have one logical dispatch and no terminal.
    pub(super) fn require_parked(
        &self,
        prompt_id: &tau_proto::AgentPromptId,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let counts = self.counts(prompt_id);
        if counts
            != (LifecycleCounts {
                created: 1,
                submitted: 1,
                finished: 0,
                retries: 1,
            })
        {
            return Err(format!("unexpected parked P1 lifecycle {counts:?}").into());
        }
        Ok(())
    }

    /// Requires one successful logical completion for one prompt.
    pub(super) fn require_finished(
        &self,
        prompt_id: &tau_proto::AgentPromptId,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let counts = self.counts(prompt_id);
        if counts.created != 1 || counts.submitted != 1 || counts.finished != 1 {
            return Err(format!("unexpected finished lifecycle {counts:?}").into());
        }
        Ok(())
    }

    /// Requires the exact two-prompt lifecycle, including retry only for P1.
    pub(super) fn require_exact_totals(
        &self,
        p1: &tau_proto::AgentPromptId,
        p2: &tau_proto::AgentPromptId,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.prompts.len() != 2
            || self.counts(p1)
                != (LifecycleCounts {
                    created: 1,
                    submitted: 1,
                    finished: 1,
                    retries: 1,
                })
            || self.counts(p2)
                != (LifecycleCounts {
                    created: 1,
                    submitted: 1,
                    finished: 1,
                    retries: 0,
                })
        {
            return Err(format!(
                "unexpected retry lifecycle P1={:?}, P2={:?}, identities={}",
                self.counts(p1),
                self.counts(p2),
                self.prompts.len()
            )
            .into());
        }
        Ok(())
    }

    /// Returns created, submitted, finished, and retry-update counts for one
    /// prompt.
    fn counts(&self, prompt_id: &tau_proto::AgentPromptId) -> LifecycleCounts {
        self.prompts.get(prompt_id).copied().unwrap_or_default()
    }

    /// Returns mutable counters for one logical prompt.
    fn counts_mut(&mut self, prompt_id: &tau_proto::AgentPromptId) -> &mut LifecycleCounts {
        self.prompts.entry(prompt_id.clone()).or_default()
    }
}
