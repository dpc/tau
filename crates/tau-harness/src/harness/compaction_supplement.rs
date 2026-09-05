//! Conversion of a validated local summary into its synthetic checkpoint.

use tau_proto::{ContentPart, ContextItem, ContextRole, MessageItem, ValidatedCompactionWindow};

/// Converts a typed local-summary envelope into the exact durable checkpoint.
///
/// `Ok(None)` means this is another provider's replacement format and must
/// remain provider-owned. A typed but malformed local envelope returns `Err`
/// before the resolver runs or a replacement can commit.
pub(super) fn compose(
    replacement_window: &[ContextItem],
) -> Result<Option<ValidatedCompactionWindow>, ()> {
    if replacement_window.iter().any(|item| {
        matches!(item, ContextItem::Message(message) if message.content.iter().any(|part| {
            matches!(part, ContentPart::SyntheticCompactionSummary { .. })
        }))
    }) {
        return Err(());
    }
    let Some(narrative) = local_narrative(replacement_window)? else {
        return Ok(None);
    };
    ValidatedCompactionWindow::new(vec![ContextItem::Message(MessageItem {
        role: ContextRole::User,
        content: vec![ContentPart::SyntheticCompactionSummary {
            text: narrative.to_owned(),
        }],
        phase: None,
        responses_raw_json: None,
    })])
    .map(Some)
    .map_err(|_| ())
}

fn local_narrative(replacement_window: &[ContextItem]) -> Result<Option<&str>, ()> {
    let has_local_envelope = replacement_window
        .iter()
        .any(|item| matches!(item, ContextItem::LocalCompactionNarrative(_)));
    if !has_local_envelope {
        return Ok(None);
    }
    let [ContextItem::LocalCompactionNarrative(item)] = replacement_window else {
        return Err(());
    };
    if item.narrative.trim().is_empty()
        || tau_proto::LOCAL_COMPACTION_NARRATIVE_MAX_BYTES < item.narrative.len()
        || is_reserved_whole_provenance_envelope(&item.narrative)
    {
        return Err(());
    }
    Ok(Some(&item.narrative))
}

fn is_reserved_whole_provenance_envelope(narrative: &str) -> bool {
    tau_proto::registered_payload_envelopes()
        .iter()
        .any(|family| match family.opening {
            tau_proto::PayloadEnvelopeOpening::Fixed(open) => {
                canonical_fixed_envelope(narrative, open, family.exact_close)
            }
            tau_proto::PayloadEnvelopeOpening::Attributed(open) => {
                canonical_attributed_envelope(narrative, open, family.exact_close)
            }
        })
        || [
            ("<user>", "</user>"),
            ("<message>", "</message>"),
            ("<response>", "</response>"),
            ("<prompt>", "</prompt>"),
        ]
        .into_iter()
        .any(|(open, close)| canonical_fixed_envelope(narrative, open, close))
        || [
            ("<message ", "</message>"),
            ("<tau_peer_message ", "</tau_peer_message>"),
            ("<tau_web_content ", "</tau_web_content>"),
        ]
        .into_iter()
        .any(|(open, close)| canonical_attributed_envelope(narrative, open, close))
}

fn canonical_fixed_envelope(narrative: &str, open: &str, close: &str) -> bool {
    narrative
        .strip_prefix(open)
        .and_then(|body| body.strip_suffix(close))
        .is_some_and(|body| !body.contains(close))
}

fn canonical_attributed_envelope(narrative: &str, open: &str, close: &str) -> bool {
    let Some(after_open) = narrative.strip_prefix(open) else {
        return false;
    };
    let Some((attributes, body_and_close)) = after_open.split_once('>') else {
        return false;
    };
    canonical_attributes(attributes)
        && body_and_close
            .strip_suffix(close)
            .is_some_and(|body| !body.contains(close))
}

fn canonical_attributes(mut attributes: &str) -> bool {
    let mut count = 0;
    while !attributes.is_empty() {
        let Some(equals) = attributes.find("=\"") else {
            return false;
        };
        let name = &attributes[..equals];
        if name.is_empty()
            || !name
                .as_bytes()
                .first()
                .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b':'))
        {
            return false;
        }
        let value_and_rest = &attributes[equals + 2..];
        let Some(quote) = value_and_rest.find('"') else {
            return false;
        };
        let value = &value_and_rest[..quote];
        if value
            .bytes()
            .any(|byte| matches!(byte, b'<' | b'>' | b'\'') || byte.is_ascii_control())
            || !canonical_attribute_entities(value)
        {
            return false;
        }
        count += 1;
        attributes = &value_and_rest[quote + 1..];
        if attributes.is_empty() {
            break;
        }
        let Some(rest) = attributes.strip_prefix(' ') else {
            return false;
        };
        attributes = rest;
    }
    0 < count
}

fn canonical_attribute_entities(mut value: &str) -> bool {
    while let Some((_, after_ampersand)) = value.split_once('&') {
        let Some(rest) = ["amp;", "lt;", "gt;", "quot;", "apos;"]
            .into_iter()
            .find_map(|entity| after_ampersand.strip_prefix(entity))
        else {
            return false;
        };
        value = rest;
    }
    true
}

#[cfg(test)]
mod tests;
