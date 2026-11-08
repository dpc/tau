//! Exact-close framing for harness-authored internal model input.

use std::borrow::Cow;

/// Exact opening sentinel for harness-stamped internal prompt payloads.
pub(crate) const TAU_INTERNAL_OPEN: &str = tau_proto::TAU_INTERNAL_OPEN;
/// Exact closing sentinel for harness-stamped internal prompt payloads.
pub(crate) const TAU_INTERNAL_CLOSE: &str = tau_proto::TAU_INTERNAL_CLOSE;

/// Frame one harness-authored internal payload for model-visible context.
///
/// The typed event or pending prompt that calls this function, rather than text
/// matching, establishes the envelope's provenance.
pub(crate) fn frame(body: &str) -> String {
    let body = tau_proto::TAU_INTERNAL_PAYLOAD_ENVELOPE.escape_body(body);
    format!("{TAU_INTERNAL_OPEN}{body}{TAU_INTERNAL_CLOSE}")
}

/// Neutralize the sole exact close token that could terminate a trusted
/// envelope. Apply this to every untrusted provider-visible payload.
pub(crate) fn escape_untrusted_close(body: &str) -> Cow<'_, str> {
    let mut escaped = tau_proto::TAU_INTERNAL_PAYLOAD_ENVELOPE.escape_body(body);
    for family in tau_proto::SEMANTIC_PAYLOAD_ENVELOPES {
        if escaped.contains(family.exact_close) {
            escaped = Cow::Owned(escaped.replace(family.exact_close, family.visible_close));
        }
    }
    escaped
}

/// Return the body of one exact internal envelope.
///
/// Callers use this only after typed harness provenance has selected an
/// internal prompt; payload text alone never establishes provenance.
#[cfg(test)]
pub(crate) fn body(text: &str) -> Option<&str> {
    text.strip_prefix(TAU_INTERNAL_OPEN)
        .and_then(|body| body.strip_suffix(TAU_INTERNAL_CLOSE))
        .or_else(|| {
            tau_proto::SEMANTIC_PAYLOAD_ENVELOPES
                .iter()
                .find_map(|family| {
                    let tau_proto::PayloadEnvelopeOpening::Fixed(open) = family.opening else {
                        return None;
                    };
                    text.strip_prefix(open)
                        .and_then(|body| body.strip_suffix(family.exact_close))
                })
        })
}
