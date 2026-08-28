//! Process-local Zulip asynchronous authority generations.
//!
//! These runtime-only counters preserve the existing wrapping lifecycle policy
//! while preventing configuration and registration callbacks from accepting
//! each other's authority.

macro_rules! zulip_authority_generation {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
        pub(crate) struct $name(#[doc = "The process-local counter value."] u64);

        impl $name {
            /// Advances this generation with its existing wrapping overflow behavior.
            #[must_use]
            pub(crate) const fn wrapping_next(self) -> Self {
                Self(self.0.wrapping_add(1))
            }

            /// Constructs a generation for a test-owned state fixture.
            #[cfg(test)]
            #[must_use]
            pub(crate) const fn new(value: u64) -> Self {
                Self(value)
            }
        }
    };
}

zulip_authority_generation!(
    ZulipConfigGeneration,
    "Generation of the currently applied Zulip configuration."
);
zulip_authority_generation!(
    ZulipRegistrationGeneration,
    "Generation of the current Zulip agent-registration authority."
);
