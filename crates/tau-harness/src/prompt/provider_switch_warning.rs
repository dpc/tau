//! Process-local deduplication for best-effort provider-switch diagnostics.

/// Warns once on actual omission during each destination-provider tenure.
#[derive(Debug, Default)]
pub(crate) struct ProviderSwitchWarning {
    /// Destination of the latest materialized request.
    destination: Option<tau_proto::ProviderName>,
    /// Whether this destination tenure already reported an omission.
    warned: bool,
}

impl ProviderSwitchWarning {
    /// Advances destination selection and reports whether a warning is due.
    pub(crate) fn observe(&mut self, destination: &tau_proto::ProviderName, omitted: bool) -> bool {
        if self.destination.as_ref() != Some(destination) {
            self.destination = Some(destination.clone());
            self.warned = false;
        }
        if omitted && !self.warned {
            self.warned = true;
            return true;
        }
        false
    }
}
