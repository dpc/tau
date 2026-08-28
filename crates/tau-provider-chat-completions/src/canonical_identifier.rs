//! Structured OpenAI-compatible error identifiers shared by Chat classifiers.

use serde_json::Value;
use tau_provider::retry_policy::{RetryClass, classify_error_code};

/// Canonical provider error identifiers in stable envelope precedence.
///
/// Precedence is root `code`, root `type`, `error.code`, `error.type`,
/// `response.error.code`, then `response.error.type`. Classification scans the
/// complete family: context exhaustion wins, then the first known retry class,
/// then the first opaque identifier.
pub(crate) struct CanonicalIdentifierFamily<'a> {
    /// Ordered identifiers borrowed from one provider envelope.
    identifiers: Vec<&'a str>,
}

impl<'a> CanonicalIdentifierFamily<'a> {
    /// Extract identifiers from a decoded HTTP provider envelope.
    #[must_use]
    pub(crate) fn from_http_envelope(value: &'a Value) -> Self {
        let objects = [
            Some(value),
            value.get("error"),
            value
                .get("response")
                .and_then(|response| response.get("error")),
        ];
        Self {
            identifiers: objects
                .into_iter()
                .flatten()
                .flat_map(|object| {
                    ["code", "type"]
                        .into_iter()
                        .filter_map(|field| object.get(field).and_then(Value::as_str))
                })
                .collect(),
        }
    }

    /// Extract identifiers from the provider-specific streamed `error` object.
    ///
    /// `metadata.error_type` is an established Chat-compatible wire spelling,
    /// not a recursively searched generic path.
    #[must_use]
    pub(crate) fn from_stream_error(error: &'a serde_json::Map<String, Value>) -> Self {
        Self {
            identifiers: [
                error.get("code").and_then(Value::as_str),
                error.get("type").and_then(Value::as_str),
                error
                    .get("metadata")
                    .and_then(Value::as_object)
                    .and_then(|metadata| metadata.get("error_type"))
                    .and_then(Value::as_str),
            ]
            .into_iter()
            .flatten()
            .collect(),
        }
    }

    /// Return identifiers in documented envelope precedence.
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
