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

/// Builds a bounded suffix, never allowing hints to select a directory or path.
pub(super) fn import_suffix(filename: Option<&str>, mime_type: Option<&str>) -> String {
    let basename = filename
        .unwrap_or_default()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default();
    let mut name: String = basename
        .chars()
        .take(100)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    name = name.trim_matches(['.', '_', '-']).to_owned();
    let extension = mime_type
        .and_then(|mime| mime.split(';').next())
        .and_then(|mime| {
            MEDIA_TYPES
                .iter()
                .find(|(known, _)| mime.trim().eq_ignore_ascii_case(known))
                .map(|(_, ext)| *ext)
        });
    if !name.contains('.')
        && let Some(extension) = extension
    {
        name.push('.');
        name.push_str(extension);
    }
    if name.is_empty() {
        String::new()
    } else if name.starts_with('.') {
        name
    } else {
        format!("-{name}")
    }
}
