//! Compact, display-only projection of recognized hosted web actions.

/// Describe a known action and its available query or page target. Unknown
/// fields stay opaque; this label never changes provider replay or diagnostics.
pub(super) fn display_action(action: &serde_json::Value) -> Option<tau_proto::ToolUseState> {
    let (label, target, unavailable) = match action["type"].as_str()? {
        "search" => (
            "search",
            action["query"]
                .as_str()
                .filter(|query| !query.trim().is_empty())
                .or_else(|| {
                    action["queries"]
                        .as_array()?
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .find(|query| !query.trim().is_empty())
                }),
            "query unavailable",
        ),
        "open_page" => ("open page", action["url"].as_str(), "URL unavailable"),
        "find_in_page" => ("find in page", action["url"].as_str(), "URL unavailable"),
        _ => return None,
    };
    Some(tau_proto::ToolUseState {
        args: target
            .filter(|target| !target.trim().is_empty())
            .map(bounded_target)
            .unwrap_or_else(|| unavailable.to_owned()),
        info_chips: vec![label.to_owned()],
        ..Default::default()
    })
}

/// Bound targets to 160 UTF-8 bytes, visibly escaping controls and preserving
/// whole Unicode characters and escape units when appending an ellipsis.
fn bounded_target(target: &str) -> String {
    const MAX_BYTES: usize = 160;
    let mut output = String::new();
    let mut starts = Vec::new();
    for character in target.chars() {
        let unit = if tau_proto::requires_visible_escape(character) {
            format!("\\u{{{:04X}}}", character as u32)
        } else {
            character.to_string()
        };
        if output.len() + unit.len() > MAX_BYTES {
            while output.len() + '…'.len_utf8() > MAX_BYTES {
                output.truncate(starts.pop().expect("bounded target has a preceding unit"));
            }
            output.push('…');
            break;
        }
        starts.push(output.len());
        output.push_str(&unit);
    }
    output
}
