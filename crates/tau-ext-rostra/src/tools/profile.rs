//! Local profile-read tool.

use rostra_client::{Client, ExternalEventId};
use tau_proto::ToolStarted;

use super::{ToolFailure, ToolFailureCategory, ToolTextResult, decode_args, parse_identity};
use crate::projection::{bounded_output, external, sanitize_line, truncate_chars};

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
/// Strict local profile-read arguments.
struct Args {
    /// Public Rostra identity to read.
    identity: String,
}

/// Read one locally synchronized profile.
pub(super) async fn handle(invoke: &ToolStarted, client: &Client) -> ToolTextResult {
    let args: Args = decode_args(&invoke.arguments)?;
    let identity = parse_identity(&args.identity)?;
    let Some(profile) = client.db().get_social_profile(identity).await else {
        return Err(ToolFailure::new(
            ToolFailureCategory::NotFoundLocal,
            "profile is not present in the synchronized local view",
        ));
    };
    let (avatar_mime, avatar_bytes) = profile
        .avatar
        .as_ref()
        .map_or(("-", 0), |(mime, bytes)| (mime.as_str(), bytes.len()));
    let remote = format!(
        "display_name: {}\navatar_mime: {}\navatar_bytes: {avatar_bytes}\nbio:\n{}",
        sanitize_line(&profile.display_name, 240),
        sanitize_line(avatar_mime, 128),
        truncate_chars(&profile.bio, 4_096),
    );
    bounded_output(format!(
        "identity: {identity}\nprofile_event_id: {}\n\n{}",
        ExternalEventId::new(identity, profile.event_id),
        external("profile", &remote),
    ))
}
