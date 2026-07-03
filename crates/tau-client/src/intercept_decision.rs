/// Decision returned by an intercept handler for one intercepted event.
#[derive(Default)]
pub enum InterceptDecision {
    /// Pass the original event through unchanged.
    #[default]
    Pass,
    /// Replace the intercepted event before it continues through the harness.
    Replace(Box<tau_proto::Event>),
    /// Drop the intercepted event.
    Drop,
}

impl InterceptDecision {
    /// Creates a replacement decision from an owned event.
    #[must_use]
    pub fn replace(event: tau_proto::Event) -> Self {
        Self::Replace(Box::new(event))
    }

    /// Converts the decision into the protocol reply action.
    #[must_use]
    pub(crate) fn into_action(self) -> tau_proto::InterceptAction {
        match self {
            Self::Pass => tau_proto::InterceptAction::Pass(None),
            Self::Replace(event) => tau_proto::InterceptAction::Pass(Some(event)),
            Self::Drop => tau_proto::InterceptAction::Drop,
        }
    }
}
