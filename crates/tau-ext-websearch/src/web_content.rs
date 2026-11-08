//! Typed external web results serialized by the shared tool-response contract.

use std::fmt::Write as _;

use tau_proto::{CborValue, ToolResponse, ToolResponseHeader};

use crate::{WebAdapter, WebOperation};

/// A successful external result with locally selected provenance metadata.
///
/// Serialization follows
/// [SPEC-tau-proto-provider-data].
///
/// [SPEC-tau-proto-provider-data]: ../../tau-proto/specs/SPEC-tau-proto-provider-data.md#provider-visible-tool-responses
/// Only the body comes from the remote service; it cannot supply trusted
/// headers.
pub(crate) struct WebContent<'a> {
    /// Locally selected adaptation path, not authentication of remote claims.
    pub(crate) adapter: WebAdapter,
    /// Operation performed by the adapter.
    pub(crate) operation: WebOperation,
    /// Untrusted remote text, including any provider-returned metadata.
    pub(crate) body: &'a str,
}

impl WebContent<'_> {
    /// Serialize through `ToolResponse`, preserving LF and visible control
    /// escapes.
    pub(crate) fn render(&self) -> String {
        let mut body = String::with_capacity(self.body.len());
        for character in self.body.chars() {
            if character != '\n' && tau_proto::requires_visible_escape(character) {
                let _ = write!(body, "\\u{{{:04X}}}", character as u32);
            } else {
                body.push(character);
            }
        }
        ToolResponse {
            raw: CborValue::Null,
            headers: [
                ("adapter", self.adapter.as_str()),
                ("operation", self.operation.as_str()),
                ("content_trust", "external"),
            ]
            .into_iter()
            .map(|(key, value)| ToolResponseHeader {
                key: key.to_owned(),
                value: value.to_owned(),
            })
            .collect(),
            body,
        }
        .render()
    }
}
