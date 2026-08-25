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
    let Some(narrative) = local_narrative(replacement_window)? else {
        return Ok(None);
    };
    ValidatedCompactionWindow::new(vec![ContextItem::Message(MessageItem {
        role: ContextRole::User,
        content: vec![ContentPart::Text {
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
    {
        return Err(());
    }
    Ok(Some(&item.narrative))
}

#[cfg(test)]
mod tests;
