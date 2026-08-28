//! Process-local Slack authority counters.
//!
//! These counters remain runtime-only while preventing distinct asynchronous
//! ownership domains from being confused at stale-authority boundaries.

macro_rules! slack_authority_counter {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
        pub(crate) struct $name(#[doc = "The process-local counter value."] u64);

        impl $name {
            /// Advances this counter with its existing wrapping overflow behavior.
            #[must_use]
            pub(crate) const fn wrapping_next(self) -> Self {
                Self(self.0.wrapping_add(1))
            }
        }
    };
}

slack_authority_counter!(
    SlackConnectionGeneration,
    "Generation of one Socket Mode connection owned by the socket worker."
);
impl SlackConnectionGeneration {
    /// Returns the generation at the scalar-only trace boundary.
    #[must_use]
    pub(crate) const fn get(self) -> u64 {
        self.0
    }

    /// Constructs a test connection generation.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }
}
slack_authority_counter!(
    SlackIngressEpoch,
    "Lifecycle epoch authorizing accepted ingress work."
);
slack_authority_counter!(
    SlackConfigGeneration,
    "Configuration generation authorizing Slack effects."
);
slack_authority_counter!(
    SlackAgentGeneration,
    "Per-agent routing generation authorizing Slack effects."
);
slack_authority_counter!(
    SlackSessionGeneration,
    "Harness session generation authorizing accepted sends."
);
/// Process-local latency-trace ordinal passed from the socket callback to
/// admission.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub(crate) struct SlackTraceSequence(#[doc = "The process-local trace ordinal."] u64);

impl SlackTraceSequence {
    /// Constructs the trace ordinal fetched by the socket callback.
    #[must_use]
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the ordinal at the scalar-only trace boundary.
    #[must_use]
    pub(crate) const fn get(self) -> u64 {
        self.0
    }
}
slack_authority_counter!(
    SlackSendReservation,
    "Process-local reservation preventing stale send workers from owning reused calls."
);
slack_authority_counter!(
    SlackSendWakeGeneration,
    "Wake generation that cancels delivery-worker waits."
);
slack_authority_counter!(
    SlackReactionEpoch,
    "Lifecycle epoch preventing stale reaction completions from restoring authority."
);
slack_authority_counter!(
    SlackReactionReservation,
    "Process-local reservation preventing stale reaction completions from clearing replacements."
);
impl SlackReactionReservation {
    /// Constructs a test reservation token.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }
}
