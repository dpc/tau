//! Per-use declared media hints and conservative local filename construction.

use super::*;

/// Common formats for which a declared MIME can provide a useful suffix.
const MEDIA_TYPES: &[(&str, &str)] = &[
    ("image/png", "png"),
    ("image/jpeg", "jpg"),
    ("image/gif", "gif"),
    ("image/webp", "webp"),
    ("image/svg+xml", "svg"),
    ("application/pdf", "pdf"),
    ("application/json", "json"),
    ("text/plain", "txt"),
    ("text/html", "html"),
    ("text/csv", "csv"),
    ("application/zip", "zip"),
];

/// Returns an explicit bounded declaration, otherwise a common extension hint.
pub(super) fn export_mime(invoke: &ToolStarted, filename: Option<&str>) -> Option<String> {
    tau_proto::cbor_text_field(&invoke.arguments, "mime_type")
        .filter(|value| !value.trim().is_empty())
        .map(|value| bounded_hint(&value))
        .or_else(|| {
            let extension = filename?.rsplit_once('.')?.1;
            MEDIA_TYPES
                .iter()
                .find(|(_, ext)| {
                    extension.eq_ignore_ascii_case(ext)
                        || (*ext == "jpg" && extension.eq_ignore_ascii_case("jpeg"))
                })
                .map(|(mime, _)| (*mime).to_owned())
        })
}
