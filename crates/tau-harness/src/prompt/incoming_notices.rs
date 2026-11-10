use std::collections::HashSet;

/// Reconstructed visibility within one selected provider-context window.
///
/// Replacement bytes remain opaque: only selected transcript suffix entries
/// consume scopes, so compaction rearms notices without durable tracking.
#[derive(Default)]
pub(super) struct IncomingNotices<'a> {
    /// Local incoming configuration scopes already rendered in this window.
    seen: HashSet<Scope<'a>>,
}

/// Identity of a local guidance configuration, never a remote claimed sender.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub(super) enum Scope<'a> {
    /// The receiving session's single global incoming-notice setting.
    InterSession,
    /// A stable endpoint identity namespaced by the authenticated publisher.
    Endpoint(&'a tau_proto::MessagePublisherId, &'a str),
}

impl<'a> IncomingNotices<'a> {
    /// Select the first nonempty snapshot for a scope in this context.
    pub(super) fn select(&mut self, scope: Scope<'a>, text: Option<&'a str>) -> Option<&'a str> {
        text.filter(|text| !text.is_empty() && self.seen.insert(scope))
    }
}
