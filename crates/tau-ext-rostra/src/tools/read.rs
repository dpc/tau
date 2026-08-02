//! Local post-read tool.

use std::str::FromStr as _;

use rostra_client::{Client, ExternalEventId};
use tau_proto::ToolStarted;

use super::{ToolFailure, ToolFailureCategory, ToolTextResult, decode_args};
use crate::projection::{bounded_output, external, format_tags, truncate_utf8};

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
/// Strict local post-read arguments.
struct Args {
    /// Full external post identifier.
    post_id: String,
}

/// Read one locally synchronized post.
pub(super) async fn handle(invoke: &ToolStarted, client: &Client) -> ToolTextResult {
    let args: Args = decode_args(&invoke.arguments)?;
    let requested = ExternalEventId::from_str(&args.post_id)
        .map_err(|_| ToolFailure::invalid("`post_id` is invalid"))?;
    let Some(record) = client.db().get_social_post(requested.event_id()).await else {
        return Err(ToolFailure::new(
            ToolFailureCategory::NotFoundLocal,
            "post is not present in the synchronized local view",
        ));
    };
    if record.author != requested.rostra_id() {
        return Err(ToolFailure::new(
            ToolFailureCategory::NotFoundLocal,
            "post author does not match the synchronized local record",
        ));
    }
    let (source, truncated) = truncate_utf8(
        record.content.djot_content.as_deref().unwrap_or_default(),
        crate::MAX_DJOT_BYTES,
    );
    let remote = format!(
        "persona_tags: {}\ndjot:\n{source}",
        format_tags(record.content.persona_tags())
    );
    bounded_output(format!(
        "post_id: {requested}\nauthor: {}\ntimestamp: {}\nreply_to: {}\nreply_count: {}\ndjot_truncated: {truncated}\n\n{}",
        record.author,
        record.ts,
        record
            .reply_to
            .map_or_else(|| "-".to_owned(), |id| id.to_string()),
        record.reply_count,
        external("post", &remote),
    ))
}
