//! Bounded hostile-content projection helpers.

use crate::tools::ToolFailure;

/// Exact sentinel that closes an untrusted Rostra content frame.
pub(crate) const EXTERNAL_CLOSE: &str = "</tau_rostra_content>";
const EXTERNAL_CLOSE_VISIBLE: &str = "&lt;/tau_rostra_content&gt;";
const MAX_TAGS: usize = 16;
const MAX_TAG_BYTES: usize = 512;
const MAX_OUTPUT_BYTES: usize = 128 * 1024;

/// Frame one complete remote-content section and escape its close sentinel.
pub(crate) fn external(kind: &str, value: &str) -> String {
    format!(
        "<tau_rostra_content kind=\"{kind}\" content_trust=\"external\">{}{EXTERNAL_CLOSE}",
        sanitize_external(value)
    )
}

/// Enforce the aggregate terminal-output byte cap.
pub(crate) fn bounded_output(value: String) -> Result<String, ToolFailure> {
    if MAX_OUTPUT_BYTES < value.len() {
        return Err(ToolFailure::internal());
    }
    Ok(value)
}

/// Project a bounded count and aggregate byte length of persona tags.
pub(crate) fn format_tags(tags: impl IntoIterator<Item = impl std::fmt::Display>) -> String {
    let mut value = String::new();
    for tag in tags.into_iter().take(MAX_TAGS) {
        let tag = sanitize_line(&tag.to_string(), 32);
        let separator = usize::from(!value.is_empty());
        if MAX_TAG_BYTES < value.len() + separator + tag.len() {
            break;
        }
        if !value.is_empty() {
            value.push(',');
        }
        value.push_str(&tag);
    }
    if value.is_empty() {
        "-".to_owned()
    } else {
        value
    }
}

/// Project attacker-controlled text onto one bounded line.
pub(crate) fn sanitize_line(value: &str, max_chars: usize) -> String {
    truncate_chars(value, max_chars)
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            {
                '�'
            } else {
                character
            }
        })
        .collect()
}

/// Escape controls and the exact external-content closing sentinel.
pub(crate) fn sanitize_external(value: &str) -> String {
    let escaped: String = value
        .chars()
        .flat_map(|character| {
            if tau_proto::requires_visible_escape(character) {
                format!("\\u{{{:04X}}}", character as u32)
                    .chars()
                    .collect::<Vec<_>>()
            } else {
                vec![character]
            }
        })
        .collect();
    tau_proto::escape_exact_sentinel_close(&escaped, EXTERNAL_CLOSE, EXTERNAL_CLOSE_VISIBLE)
        .into_owned()
}

/// Truncate text by Unicode scalar count.
pub(crate) fn truncate_chars(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

/// Truncate text at a valid UTF-8 boundary and report truncation.
pub(crate) fn truncate_utf8(value: &str, max_bytes: usize) -> (&str, bool) {
    if value.len() <= max_bytes {
        return (value, false);
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (&value[..end], true)
}
