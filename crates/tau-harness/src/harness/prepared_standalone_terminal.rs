//! Output-bearing standalone publications prepared before terminal effects.

use super::*;

/// The exact first outcome fact; dependent context-recovery facts remain
/// post-commit state-machine work rather than a prepared multi-event bundle.
pub(super) enum PreparedStandaloneTerminal {
    /// Validated replacement boundary, absent if its transaction disappeared.
    Accepted(Option<(tau_core::AgentEventParent, Box<Event>)>),
    /// Existing rejection classification and its immediate failure, if any.
    Rejected {
        /// Diagnostic and context-rejection publication classification.
        reason: StandaloneCompactionRejection,
        /// Exact immediate failure; context rejection instead publishes the
        /// canonical provider response and derives its failure after commit.
        failure: Option<Box<prepared_standalone_failure::PreparedStandaloneFailure>>,
    },
}

impl Harness {
    /// Materialize the output-bearing outcome without accounting, ownership
    /// cleanup, watcher changes, or successor identity consumption.
    pub(super) fn prepare_standalone_terminal(
        &self,
        cid: &AgentId,
        response: &ProviderResponseFinished,
        plan: StandaloneCompactionTerminalPlan,
    ) -> PreparedStandaloneTerminal {
        match plan {
            StandaloneCompactionTerminalPlan::Accepted(window) => {
                PreparedStandaloneTerminal::Accepted(
                    self.standalone_compaction_boundary(cid, response, &window)
                        .map(|(_, parent, event)| (parent, Box::new(event))),
                )
            }
            StandaloneCompactionTerminalPlan::Rejected(reason) => {
                let failure =
                    (!matches!(reason, StandaloneCompactionRejection::ContextWindowExceeded))
                        .then(|| {
                            self.prepare_standalone_failure(cid, response, reason.durable_reason())
                        })
                        .flatten()
                        .map(Box::new);
                PreparedStandaloneTerminal::Rejected { reason, failure }
            }
        }
    }
}
