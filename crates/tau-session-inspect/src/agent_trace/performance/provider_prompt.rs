//! One content-free provider-prompt correlation.

#[cfg(test)]
mod tests;

use tau_core::PersistedAgentEventSeq;
use tau_proto::{
    AgentId, AgentPromptId, EstimatedApiCost, ModelId, ProviderResponseFinished, UnixMicros,
};

use super::provider_prompt_record::ProviderPromptRecord;
use super::summary::Summary;
use super::usage::Usage;
use crate::InspectError;

/// Content-free fields retained from one terminal provider response.
struct TerminalEvidence {
    /// Owning journal sequence.
    journal_seq: PersistedAgentEventSeq,
    /// Terminal record timestamp.
    recorded_at: UnixMicros,
    /// Number of preceding journal wall-clock regressions.
    clock_regressions: u64,
    /// Optional response-local token evidence.
    usage: Option<Usage>,
    /// Optional stored estimated cost.
    cost: Option<EstimatedApiCost>,
}

/// Durable lifecycle evidence accumulated for one prompt ID.
pub(super) struct ProviderPrompt {
    /// Canonical prompt materialization.
    prompt_started: PromptStart,
    /// Model captured by canonical materialization.
    model: ModelId,
    /// Canonical terminal accounting evidence.
    terminal: Option<TerminalEvidence>,
}

/// Required materialization boundary and its clock-regression epoch.
#[derive(Clone, Copy)]
struct PromptStart {
    /// Owning journal sequence.
    journal_seq: PersistedAgentEventSeq,
    /// Wall-clock sample recorded at append invocation.
    recorded_at: UnixMicros,
    /// Number of preceding comparable wall-clock regressions.
    clock_regressions: u64,
}

impl ProviderPrompt {
    /// Creates a correlation from its required materialization fact.
    pub(super) fn new(
        journal_seq: PersistedAgentEventSeq,
        at: UnixMicros,
        clock_regressions: u64,
        model: ModelId,
    ) -> Self {
        Self {
            prompt_started: PromptStart {
                journal_seq,
                recorded_at: at,
                clock_regressions,
            },
            model,
            terminal: None,
        }
    }

    /// Records one unique content-free terminal projection.
    pub(super) fn set_terminal(
        &mut self,
        journal_seq: PersistedAgentEventSeq,
        at: UnixMicros,
        clock_regressions: u64,
        response: ProviderResponseFinished,
    ) -> bool {
        let usage = response.usage.map(|usage| {
            Usage::new(
                usage.prompt_sent_tokens,
                usage.prompt_cached_tokens,
                usage.response_received_tokens,
            )
        });
        self.terminal
            .replace(TerminalEvidence {
                journal_seq,
                recorded_at: at,
                clock_regressions,
                usage,
                cost: response.estimated_api_cost_increment,
            })
            .is_none()
    }

    /// Projects this correlation and adds its exact evidence to the summary.
    pub(super) fn project<'a>(
        &'a self,
        agent_id: &'a AgentId,
        prompt_id: &'a AgentPromptId,
        origin: Option<UnixMicros>,
        summary: &mut Summary,
    ) -> Result<ProviderPromptRecord<'a>, InspectError> {
        summary.add_occurrence()?;
        let start = self.start();
        let terminal_at = self.terminal.as_ref().map(|terminal| terminal.recorded_at);
        let elapsed = self.terminal.as_ref().and_then(|terminal| {
            elapsed_without_regression(start, terminal.recorded_at, terminal.clock_regressions)
        });
        if let Some(elapsed) = elapsed {
            summary.add_elapsed(elapsed)?;
        }
        let (usage, cost) = self.terminal.as_ref().map_or((None, None), |terminal| {
            (terminal.usage.as_ref(), terminal.cost)
        });
        if self.terminal.is_some() {
            summary.add_terminal()?;
            if let Some(usage) = usage {
                summary.add_usage(usage)?;
            }
            if let Some(cost) = cost {
                summary.add_cost(cost.as_picodollars())?;
            }
        }
        Ok(ProviderPromptRecord {
            record_type: "provider_prompt",
            agent_id,
            agent_prompt_id: prompt_id,
            model: &self.model,
            journal_seq: start.journal_seq.get(),
            terminal_journal_seq: self
                .terminal
                .as_ref()
                .map(|terminal| terminal.journal_seq.get()),
            at_us: relative_time(Some(start.recorded_at), origin),
            terminal_at_us: relative_time(terminal_at, origin),
            recorded_at_wall_elapsed_us: elapsed,
            terminal_present: self.terminal.is_some(),
            prompt_sent_tokens: usage.map(Usage::sent),
            prompt_cached_tokens: usage.map(Usage::cached),
            response_received_tokens: usage.map(Usage::received),
            estimated_api_cost_picodollars: cost.map(EstimatedApiCost::as_picodollars),
        })
    }

    fn start(&self) -> PromptStart {
        self.prompt_started
    }

    /// Returns the authoritative start sequence for row ordering.
    pub(super) fn journal_seq(&self) -> u64 {
        self.prompt_started.journal_seq.get()
    }
}

fn relative_time(at: Option<UnixMicros>, origin: Option<UnixMicros>) -> Option<u64> {
    at.filter(|at| at.get() != 0)
        .zip(origin)
        .and_then(|(at, origin)| at.get().checked_sub(origin.get()))
}

fn valid_elapsed(start: UnixMicros, terminal: UnixMicros) -> Option<u64> {
    (start.get() != 0 && terminal.get() != 0)
        .then(|| terminal.get().checked_sub(start.get()))
        .flatten()
}

fn elapsed_without_regression(
    start: PromptStart,
    terminal: UnixMicros,
    terminal_regressions: u64,
) -> Option<u64> {
    (start.clock_regressions == terminal_regressions)
        .then(|| valid_elapsed(start.recorded_at, terminal))
        .flatten()
}
