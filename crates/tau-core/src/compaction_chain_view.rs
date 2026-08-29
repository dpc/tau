//! Replay-derived observability for one explicitly linked compaction chain.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use tau_proto::{
    CompactionTransactionId, EstimatedApiCost, Event, ProviderAttempt, StandaloneCompactionTrigger,
    StandaloneExecutionUsage, UnixMicros,
};

use crate::PersistedAgentEventSeq;

/// Whether the latest durable transaction proves the chain has stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompactionChainCompletion {
    /// A durable decision exists but its transaction has not started.
    AwaitingStart,
    /// The latest transaction has no durable terminal boundary.
    InFlight,
    /// A successful boundary awaits its same-transaction inference checkpoint,
    /// or an explicit terminal requires another linked transaction.
    AwaitingContinuation,
    /// The latest transaction has a terminal boundary and owes no successor.
    Complete,
}

/// What the chain's durable timestamps establish about elapsed time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompactionChainElapsed {
    /// No pass has started, or at least one required durable timestamp is the
    /// legacy/missing zero value.
    UnknownMissingTimestamp,
    /// Ordered timestamps produced an ordinary nonnegative duration.
    Observed {
        /// Time from the first pass start through the latest chain fact.
        duration: Duration,
    },
    /// Wall time moved backwards; `duration` is bounded with saturating
    /// subtraction.
    SaturatedClockReversal {
        /// Bounded time from the first pass start to the latest chain fact.
        duration: Duration,
    },
}

/// Knowledge state for the chain's committed estimated provider cost.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompactionChainEstimatedCost {
    /// Every effective per-attempt observation has known usage and exact cost.
    Known(EstimatedApiCost),
    /// An effective observation has unknown usage or a possibly dispatched pass
    /// has no canonical accounting observation.
    Unknown,
}

/// Non-authoritative observability derived from one canonical durable chain.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionChainView {
    /// First transaction or automatic decision in the explicit predecessor
    /// chain.
    pub root_transaction_id: CompactionTransactionId,
    /// Exact transaction or decision requested by the caller.
    pub latest_transaction_id: CompactionTransactionId,
    /// Saturating count of durable standalone-compaction starts in the chain.
    pub pass_count: u64,
    /// Whether the latest durable boundary completes or continues the chain.
    pub completion: CompactionChainCompletion,
    /// Elapsed time through the latest durable chain fact, including its
    /// quality.
    pub elapsed: CompactionChainElapsed,
    /// Saturating fold of effective committed per-attempt accounting.
    pub estimated_cost: CompactionChainEstimatedCost,
}

/// One exact effective accounting observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AttemptCost {
    /// Canonical usage was explicitly unknown.
    Unknown,
    /// Canonical usage carried this exact estimated cost.
    Known(EstimatedApiCost),
}

/// One timestamped durable fact relevant to elapsed-chain observability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TimedFact {
    /// Agent-journal occurrence order.
    seq: PersistedAgentEventSeq,
    /// Canonical append timestamp.
    recorded_at: UnixMicros,
}

/// Whether one durable start could have reached provider dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DispatchPossibility {
    /// A typed preflight start proves that no provider request can occur.
    NeverDispatched,
    /// The start may have reached provider dispatch and therefore owes
    /// accounting.
    MayDispatch,
}

/// Immutable facts from the sole durable transaction start.
#[derive(Clone, Debug, Eq, PartialEq)]
struct StartFacts {
    /// Explicit canonical predecessor, if any.
    predecessor: Option<CompactionTransactionId>,
    /// Whether this transaction can have reached provider dispatch.
    initial_dispatch: DispatchPossibility,
    /// Whether success still owes a continuation or checkpoint.
    resumes_inference: bool,
    /// Exact durable start occurrence and timestamp.
    timed_fact: TimedFact,
}

/// Replay-derived facts for one decision/transaction.
#[derive(Clone, Debug, Default, PartialEq)]
struct TransactionFacts {
    /// The transaction's sole durable start, when committed.
    start: Option<StartFacts>,
    /// Explicit terminal/continuation knowledge, when committed.
    boundary: Option<BoundaryFacts>,
    /// Relevant durable observations in journal order.
    timed_facts: Vec<TimedFact>,
    /// Effective costs keyed by finite provider attempt.
    attempts: HashMap<ProviderAttempt, AttemptCost>,
}

/// Terminal knowledge representable by a committed compaction boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BoundaryState {
    /// Success awaits a checkpoint or an explicit chain successor.
    AwaitingContinuation,
    /// The durable boundary completes the requested chain prefix.
    Complete,
}

/// Immutable facts established together by one terminal boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BoundaryFacts {
    /// Whether the boundary completes or continues the chain.
    state: BoundaryState,
    /// Whether the boundary proves no provider dispatch occurred.
    proved_no_dispatch: bool,
}

/// Complete non-persisted compaction observability index rebuilt by replay.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct CompactionChainIndex {
    /// Facts keyed by canonical transaction identity.
    transactions: HashMap<CompactionTransactionId, TransactionFacts>,
    /// Unique compact-prompt ownership established by durable starts.
    prompts: HashMap<tau_proto::AgentPromptId, CompactionTransactionId>,
}

impl CompactionChainIndex {
    /// Observe one already-validated durable record without granting authority.
    pub(crate) fn observe(
        &mut self,
        seq: PersistedAgentEventSeq,
        recorded_at: UnixMicros,
        event: &Event,
    ) {
        let timed_fact = TimedFact { seq, recorded_at };
        match event {
            Event::ProviderResponseFinished(response) => {
                if let Some(decision) = &response.automatic_compaction_decision {
                    self.observe_timed(&decision.transaction_id, timed_fact);
                }
                if let Some(transaction_id) = self.prompts.get(&response.agent_prompt_id).cloned() {
                    self.observe_timed(&transaction_id, timed_fact);
                }
            }
            Event::AgentPromptTerminated(terminated) => {
                if let Some(decision) = &terminated.automatic_compaction_decision {
                    self.observe_timed(&decision.transaction_id, timed_fact);
                }
            }
            Event::AgentStandaloneCompactionStarted(started) => {
                let dispatch = if matches!(
                    started.trigger,
                    StandaloneCompactionTrigger::AutomaticPreflightFailure { .. }
                        | StandaloneCompactionTrigger::ReactivePreflightFailure { .. }
                ) {
                    DispatchPossibility::NeverDispatched
                } else {
                    DispatchPossibility::MayDispatch
                };
                self.prompts.insert(
                    started.compact_prompt_id.clone(),
                    started.transaction_id.clone(),
                );
                let facts = self
                    .transactions
                    .entry(started.transaction_id.clone())
                    .or_default();
                facts.start = Some(StartFacts {
                    predecessor: explicit_predecessor(started),
                    initial_dispatch: dispatch,
                    resumes_inference: started.resume_through.is_some(),
                    timed_fact,
                });
                facts.timed_facts.push(timed_fact);
            }
            Event::AgentPromptStarted(started) => {
                if let Some(transaction_id) = self.prompts.get(&started.agent_prompt_id).cloned() {
                    self.observe_timed(&transaction_id, timed_fact);
                }
            }
            Event::ProviderStandaloneExecutionAccounted(accounted) => {
                let facts = self
                    .transactions
                    .entry(accounted.transaction_id.clone())
                    .or_default();
                facts.attempts.insert(
                    accounted.logical_attempt,
                    attempt_cost(&accounted.usage, accounted.estimated_api_cost_increment),
                );
                facts.timed_facts.push(timed_fact);
            }
            Event::ProviderStandaloneExecutionAccountingCorrected(corrected) => {
                let facts = self
                    .transactions
                    .entry(corrected.transaction_id.clone())
                    .or_default();
                facts.attempts.insert(
                    corrected.logical_attempt,
                    attempt_cost(&corrected.usage, corrected.estimated_api_cost_increment),
                );
                facts.timed_facts.push(timed_fact);
            }
            Event::AgentStandaloneCompactionFailed(failed) => {
                let facts = self
                    .transactions
                    .entry(failed.transaction_id.clone())
                    .or_default();
                let proved_no_dispatch = matches!(
                    failed.reason,
                    tau_proto::StandaloneCompactionFailureReason::RouteFailed
                        | tau_proto::StandaloneCompactionFailureReason::PrefixTooLarge
                ) && facts.attempts.is_empty();
                facts.boundary = Some(BoundaryFacts {
                    state: if failed.context_retreat.is_some() {
                        BoundaryState::AwaitingContinuation
                    } else {
                        BoundaryState::Complete
                    },
                    proved_no_dispatch,
                });
                facts.timed_facts.push(timed_fact);
            }
            Event::AgentCompacted(compacted) => {
                let Some(transaction_id) = compacted.transaction_id.as_ref() else {
                    return;
                };
                let facts = self.transactions.entry(transaction_id.clone()).or_default();
                facts.boundary = Some(BoundaryFacts {
                    state: if facts
                        .start
                        .as_ref()
                        .is_some_and(|start| start.resumes_inference)
                    {
                        BoundaryState::AwaitingContinuation
                    } else {
                        BoundaryState::Complete
                    },
                    proved_no_dispatch: false,
                });
                facts.timed_facts.push(timed_fact);
            }
            Event::AgentInferenceDispatchStarted(checkpoint) => {
                if let Some(transaction_id) = checkpoint.transaction_id.as_ref() {
                    let facts = self.transactions.entry(transaction_id.clone()).or_default();
                    facts.boundary = Some(BoundaryFacts {
                        state: BoundaryState::Complete,
                        proved_no_dispatch: false,
                    });
                    facts.timed_facts.push(timed_fact);
                }
            }
            _ => {}
        }
    }

    /// Add one correlated durable timestamp without changing lifecycle state.
    fn observe_timed(&mut self, transaction_id: &CompactionTransactionId, fact: TimedFact) {
        self.transactions
            .entry(transaction_id.clone())
            .or_default()
            .timed_facts
            .push(fact);
    }

    /// Return the explicit predecessor lineage ending at `latest`.
    fn lineage(&self, latest: &CompactionTransactionId) -> Option<Vec<&TransactionFacts>> {
        let mut reversed = Vec::new();
        let mut seen = HashSet::new();
        let mut current = latest;
        loop {
            if !seen.insert(current.clone()) {
                return None;
            }
            let facts = self.transactions.get(current)?;
            reversed.push(facts);
            let Some(predecessor) = facts
                .start
                .as_ref()
                .and_then(|start| start.predecessor.as_ref())
            else {
                break;
            };
            current = predecessor;
        }
        reversed.reverse();
        Some(reversed)
    }
}

/// Select the sole canonical predecessor field without adjacency inference.
fn explicit_predecessor(
    started: &tau_proto::AgentStandaloneCompactionStarted,
) -> Option<CompactionTransactionId> {
    match &started.trigger {
        StandaloneCompactionTrigger::AutomaticContinuation {
            previous_transaction_id,
        }
        | StandaloneCompactionTrigger::AutomaticPreflightFailure {
            previous_transaction_id: Some(previous_transaction_id),
            ..
        } => Some(previous_transaction_id.clone()),
        StandaloneCompactionTrigger::AutomaticContextRetreat {
            failed_transaction_id,
            ..
        } => Some(failed_transaction_id.clone()),
        _ => started.supersedes.clone(),
    }
}

/// Convert validated known-or-unknown accounting into its effective cost.
fn attempt_cost(
    usage: &StandaloneExecutionUsage,
    estimated_cost: Option<EstimatedApiCost>,
) -> AttemptCost {
    match (usage, estimated_cost) {
        (StandaloneExecutionUsage::Known(_), Some(cost)) => AttemptCost::Known(cost),
        _ => AttemptCost::Unknown,
    }
}

/// Fold cost over the exact effective attempts in one lineage.
fn estimated_cost(lineage: &[&TransactionFacts]) -> CompactionChainEstimatedCost {
    let mut total = 0_u64;
    for facts in lineage {
        if facts.start.as_ref().is_some_and(|start| {
            start.initial_dispatch == DispatchPossibility::MayDispatch
                && !facts
                    .boundary
                    .is_some_and(|boundary| boundary.proved_no_dispatch)
                && facts.attempts.is_empty()
        }) {
            return CompactionChainEstimatedCost::Unknown;
        }
        for attempt in facts.attempts.values() {
            let AttemptCost::Known(cost) = attempt else {
                return CompactionChainEstimatedCost::Unknown;
            };
            total = total.saturating_add(cost.as_picodollars());
        }
    }
    CompactionChainEstimatedCost::Known(EstimatedApiCost::from_picodollars(total))
}

/// Fold elapsed time while retaining missing-clock and reversal quality.
fn elapsed(lineage: &[&TransactionFacts]) -> CompactionChainElapsed {
    let Some(first_start) = lineage
        .iter()
        .find_map(|transaction| transaction.start.as_ref().map(|start| start.timed_fact))
    else {
        return CompactionChainElapsed::UnknownMissingTimestamp;
    };
    let mut facts = lineage
        .iter()
        .flat_map(|transaction| transaction.timed_facts.iter())
        .filter(|fact| first_start.seq.get() <= fact.seq.get())
        .copied()
        .collect::<Vec<_>>();
    facts.sort_unstable_by_key(|fact| fact.seq.get());
    if first_start.recorded_at.get() == 0 || facts.iter().any(|fact| fact.recorded_at.get() == 0) {
        return CompactionChainElapsed::UnknownMissingTimestamp;
    }
    let reversed = facts
        .windows(2)
        .any(|pair| pair[1].recorded_at < pair[0].recorded_at);
    let latest = facts
        .last()
        .map_or(first_start.recorded_at, |fact| fact.recorded_at);
    let micros = latest.get().saturating_sub(first_start.recorded_at.get());
    if reversed {
        CompactionChainElapsed::SaturatedClockReversal {
            duration: Duration::from_micros(micros),
        }
    } else {
        CompactionChainElapsed::Observed {
            duration: Duration::from_micros(micros),
        }
    }
}

impl CompactionChainIndex {
    /// Derive the complete view from immutable replay-derived facts.
    pub(crate) fn derive(&self, latest: &CompactionTransactionId) -> Option<CompactionChainView> {
        let lineage = self.lineage(latest)?;
        let pass_count =
            u64::try_from(lineage.iter().filter(|facts| facts.start.is_some()).count())
                .unwrap_or(u64::MAX);
        let root_transaction_id = {
            let mut current = latest;
            while let Some(predecessor) = self
                .transactions
                .get(current)
                .and_then(|facts| facts.start.as_ref())
                .and_then(|start| start.predecessor.as_ref())
            {
                current = predecessor;
            }
            current.clone()
        };
        let latest_facts = self.transactions.get(latest)?;
        let completion = match latest_facts.boundary.map(|boundary| boundary.state) {
            Some(BoundaryState::AwaitingContinuation) => {
                CompactionChainCompletion::AwaitingContinuation
            }
            Some(BoundaryState::Complete) => CompactionChainCompletion::Complete,
            None if latest_facts.start.is_some() => CompactionChainCompletion::InFlight,
            None => CompactionChainCompletion::AwaitingStart,
        };
        Some(CompactionChainView {
            root_transaction_id,
            latest_transaction_id: latest.clone(),
            pass_count,
            completion,
            elapsed: elapsed(&lineage),
            estimated_cost: estimated_cost(&lineage),
        })
    }
}

#[cfg(test)]
mod tests;
