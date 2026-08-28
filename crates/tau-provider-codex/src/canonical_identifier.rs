//! Structured provider error identifiers shared by transport classifiers.

use serde_json::Value;
use tau_provider::retry_policy::{RetryClass, classify_error_code};

/// Canonical provider error identifiers in stable envelope precedence.
///
/// Precedence is root `code`, root `type`, `error.code`, `error.type`,
/// `response.error.code`, then `response.error.type`. Classification scans the
/// whole family: context exhaustion wins over every other identifier, then the
/// first known transient class, then the first identifier is retained as opaque
/// operation-specific evidence. Retry policy interprets that final identifier.
pub(crate) struct CanonicalIdentifierFamily<'a> {
    /// Ordered identifier candidates borrowed from one provider envelope.
    identifiers: Vec<&'a str>,
}

impl<'a> CanonicalIdentifierFamily<'a> {
    /// Extract canonical identifiers from one decoded provider envelope.
    #[must_use]
    pub(crate) fn from_value(value: &'a Value) -> Self {
        Self::extract(value, true)
    }

    /// Extract identifiers from one streaming event while keeping its root
    /// `type` as event-language evidence rather than an error identifier.
    #[must_use]
    pub(crate) fn from_provider_event(value: &'a Value) -> Self {
        Self::extract(value, false)
    }

    /// Extract one family with an explicit root-type policy.
    fn extract(value: &'a Value, include_root_type: bool) -> Self {
        let objects = [
            Some(value),
            value.get("error"),
            value
                .get("response")
                .and_then(|response| response.get("error")),
        ];
        let identifiers = objects
            .into_iter()
            .enumerate()
            .filter_map(|(index, object)| object.map(|object| (index, object)))
            .flat_map(|(index, object)| {
                ["code", "type"]
                    .into_iter()
                    .filter(move |field| include_root_type || index != 0 || *field != "type")
                    .filter_map(|field| object.get(field).and_then(Value::as_str))
            })
            .collect();
        Self { identifiers }
    }

    /// Return all identifiers in documented envelope precedence.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &'a str> + '_ {
        self.identifiers.iter().copied()
    }

    /// Select the identifier that owns semantic classification.
    #[must_use]
    pub(crate) fn classified(&self) -> Option<&'a str> {
        self.iter()
            .find(|identifier| *identifier == "context_length_exceeded")
            .or_else(|| {
                self.iter()
                    .find(|identifier| classify_error_code(identifier) != RetryClass::Unknown)
            })
            .or_else(|| self.iter().next())
    }
}

#[cfg(test)]
#[path = "canonical_identifier_tests.rs"]
mod tests;
