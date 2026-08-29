//! Owns watch status classification and user/assistant notification fanout.
//!
//! Registry membership and publication commit authority remain separate.

use super::*;

#[cfg(test)]
thread_local! {
    /// Number of prompt-text copies made for watch fanout on this test thread.
    static WATCH_PROMPT_TEXT_CLONE_COUNT: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
}

/// Watch topology, delivery deduplication, and endpoint-retirement state.
#[derive(Default)]
pub(crate) struct AgentWatchState {
    /// Watched agent ids keyed by watcher public agent id.
    pub(crate) forward: HashMap<String, BTreeSet<String>>,
    /// Watcher agent ids keyed by watched public agent id.
    pub(crate) reverse: HashMap<String, BTreeSet<String>>,
    /// Subscription identity for every directed watch relation.
    pub(crate) subscriptions: HashMap<(String, String), String>,
    /// Current sanitized provider-work snapshot by watched agent id.
    pub(crate) provider_status: HashMap<String, tau_proto::AgentWatchProviderStatusNotification>,
    /// Bounded provider-status delivery state by subscription.
    pub(crate) provider_deliveries: HashMap<String, AgentWatchProviderDeliveries>,
    /// Long-wait crossings awaiting bounded durable materialization.
    pub(super) pending_long_wait_notifications:
        VecDeque<subagents_tool::PendingLongWaitNotifications>,
    /// Remaining long-wait materialization budget in the active scheduler call.
    pub(super) long_wait_materialization_budget: Option<usize>,
    /// Selected reason for an unexpected watched endpoint unload.
    pub(super) pending_unload_reasons: HashMap<String, tau_proto::AgentWatchLifecycleReason>,
    /// Endpoint ids whose pending unload is expected completion or cleanup.
    pub(super) expected_unloads: HashSet<String>,
    /// Unexpected retirements awaiting watcher lifecycle appends.
    pub(super) pending_retirements: HashMap<String, subagents_tool::PendingWatchRetirement>,
}

/// Dedupe projection for one provider status notification.
///
/// Attempts and retry delays intentionally do not participate: repeated
/// same-category retries update the snapshot without prompting again.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum AgentWatchProviderDeliveryKind {
    /// Retrying with the enclosed sanitized category.
    Retrying(tau_proto::AgentWatchProviderCategory),
    /// Recovering from a context-window rejection.
    RecoveringContext,
    /// Blocked with the enclosed sanitized category.
    Blocked(tau_proto::AgentWatchProviderCategory),
    /// Dispatch is uncertain for the enclosed sanitized category.
    DispatchUncertain(tau_proto::AgentWatchProviderCategory),
    /// Terminal with the enclosed sanitized failure kind.
    TerminalError(tau_proto::ProviderFailureKind),
    /// Terminal incomplete output with the enclosed sanitized category.
    TerminalIncomplete(tau_proto::AgentWatchProviderCategory),
}

impl From<&tau_proto::AgentWatchProviderState> for AgentWatchProviderDeliveryKind {
    fn from(state: &tau_proto::AgentWatchProviderState) -> Self {
        match state {
            tau_proto::AgentWatchProviderState::Retrying { category, .. } => {
                Self::Retrying(*category)
            }
            tau_proto::AgentWatchProviderState::RecoveringContext { .. } => Self::RecoveringContext,
            tau_proto::AgentWatchProviderState::Blocked { category } => Self::Blocked(*category),
            tau_proto::AgentWatchProviderState::DispatchUncertain { category } => {
                Self::DispatchUncertain(*category)
            }
            tau_proto::AgentWatchProviderState::TerminalError { failure_kind, .. } => {
                Self::TerminalError(*failure_kind)
            }
            tau_proto::AgentWatchProviderState::TerminalIncomplete { category, .. } => {
                Self::TerminalIncomplete(*category)
            }
        }
    }
}

pub(super) fn watch_category_for_retry(
    category: tau_proto::ProviderRetryCategory,
) -> tau_proto::AgentWatchProviderCategory {
    match category {
        tau_proto::ProviderRetryCategory::Transport => {
            tau_proto::AgentWatchProviderCategory::Transport
        }
        tau_proto::ProviderRetryCategory::Overload => {
            tau_proto::AgentWatchProviderCategory::Overload
        }
        tau_proto::ProviderRetryCategory::Throttle => {
            tau_proto::AgentWatchProviderCategory::Throttle
        }
        tau_proto::ProviderRetryCategory::UsageWindow => {
            tau_proto::AgentWatchProviderCategory::UsageWindow
        }
        tau_proto::ProviderRetryCategory::Account => tau_proto::AgentWatchProviderCategory::Account,
        tau_proto::ProviderRetryCategory::Auth => tau_proto::AgentWatchProviderCategory::Auth,
        tau_proto::ProviderRetryCategory::Unknown => tau_proto::AgentWatchProviderCategory::Unknown,
    }
}

impl Harness {
    /// Reports whether the runtime reverse index currently has a prompt
    /// watcher.
    ///
    /// This is only an allocation-avoidance probe. Delivery still takes its
    /// ordinary post-commit watcher snapshot, so it retains the established
    /// publication ordering and topology semantics.
    pub(super) fn has_watchers_for_agent(&self, agent_id: &str) -> bool {
        self.agent_runtime
            .agent_watch
            .reverse
            .get(agent_id)
            .is_some_and(|watchers| !watchers.is_empty())
    }

    /// Copies prompt text only when the reverse watch index can fan it out.
    pub(super) fn clone_prompt_text_for_watch_notification(&self, text: &str) -> String {
        #[cfg(test)]
        WATCH_PROMPT_TEXT_CLONE_COUNT.with(|count| count.set(count.get().saturating_add(1)));

        text.to_owned()
    }

    /// Resets the test-thread counter for prompt-text copies made by watch
    /// fanout.
    #[cfg(test)]
    pub(super) fn reset_watch_prompt_text_clone_count_for_test(&self) {
        WATCH_PROMPT_TEXT_CLONE_COUNT.with(|count| count.set(0));
    }

    /// Returns prompt-text copies made by watch fanout on the current test
    /// thread.
    #[cfg(test)]
    pub(super) fn watch_prompt_text_clone_count_for_test(&self) -> usize {
        WATCH_PROMPT_TEXT_CLONE_COUNT.with(|count| count.get())
    }

    pub(super) fn notify_agent_watchers_about_user_prompt(&mut self, agent_id: &str, text: &str) {
        for watcher_id in self.watchers_for_agent(agent_id) {
            let Some(sender_cid) = self
                .agent_runtime
                .agent_registry
                .agent_routes
                .get(agent_id)
                .cloned()
            else {
                return;
            };
            if self
                .publish_agent_delivery_from_agent(
                    &sender_cid,
                    watcher_id.clone(),
                    text.to_owned(),
                    tau_proto::AgentMessageKind::WatchPrompt,
                )
                .is_err()
            {
                self.prune_agent_watch(&watcher_id, agent_id);
            }
        }
    }

    /// Publish one accepted ordinary final to each current watcher after the
    /// provider response itself has committed.
    pub(super) fn notify_agent_watchers_about_response(&mut self, cid: &AgentId, message: String) {
        let Some(agent_id) = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| agent.identity.agent_id.clone())
        else {
            return;
        };
        for watcher_id in self.watchers_for_agent(&agent_id) {
            if self
                .publish_agent_watch_response_from_agent(cid, watcher_id.clone(), message.clone())
                .is_err()
            {
                self.prune_agent_watch(&watcher_id, &agent_id);
            }
        }
    }
}
