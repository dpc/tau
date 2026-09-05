//! Exact-close framing for harness-authored internal model input.

/// Exact opening sentinel for harness-stamped internal prompt payloads.
pub(crate) const TAU_INTERNAL_OPEN: &str = tau_proto::TAU_INTERNAL_OPEN;
/// Exact closing sentinel for harness-stamped internal prompt payloads.
pub(crate) const TAU_INTERNAL_CLOSE: &str = tau_proto::TAU_INTERNAL_CLOSE;
/// Visible replacement for a closing sentinel collision in an internal body.
pub(crate) const TAU_INTERNAL_CLOSE_VISIBLE: &str = "&lt;/tau_internal&gt;";

/// Frame one harness-authored internal payload for model-visible context.
///
/// The typed event or pending prompt that calls this function, rather than text
/// matching, establishes the envelope's provenance.
pub(crate) fn frame(body: &str) -> String {
    let body = escape_untrusted_close(body);
    format!("{TAU_INTERNAL_OPEN}{body}{TAU_INTERNAL_CLOSE}")
}

/// Neutralize the sole exact close token that could terminate a trusted
/// envelope. Apply this to every untrusted provider-visible payload.
pub(crate) fn escape_untrusted_close(body: &str) -> std::borrow::Cow<'_, str> {
    tau_proto::TAU_INTERNAL_PAYLOAD_ENVELOPE.escape_body(body)
}

/// Return the body of one exact internal envelope.
///
/// Callers use this only after typed harness provenance has selected an
/// internal prompt; payload text alone never establishes provenance.
#[cfg(test)]
pub(crate) fn body(text: &str) -> Option<&str> {
    text.strip_prefix(TAU_INTERNAL_OPEN)
        .and_then(|body| body.strip_suffix(TAU_INTERNAL_CLOSE))
}
