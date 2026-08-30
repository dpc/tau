//! Local status tool.

#[cfg(test)]
mod tests;

use rostra_client::Client;
use tau_proto::ToolStarted;

use super::{ToolTextResult, decode_args};
use crate::projection::bounded_output;

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
/// Empty strict argument object for local status.
struct Args {}

/// Report conservative local synchronization state.
pub(super) async fn handle(invoke: &ToolStarted, client: &Client) -> ToolTextResult {
    let _: Args = decode_args(&invoke.arguments)?;
    let identity = client.rostra_id();
    let wot = client.self_wot_subscribe().snapshot();
    render(identity, wot.followees.len(), wot.extended.len())
}

/// Formats status from counts captured from one retained graph snapshot.
fn render(
    identity: rostra_client::RostraId,
    direct_followees: usize,
    two_hop_identities: usize,
) -> ToolTextResult {
    bounded_output(format!(
        "identity: {identity}\nmode: local synchronized view; signing activates lazily on the first signed tool call\ntransport: relay-only Iroh peer transport; Pkarr HTTPS/DNS discovery; no direct peer-IP\ndatabase: open\nknown_direct_followees: {}\nknown_two_hop_identities: {}\nclient_state: started\nsynchronization_health: unknown",
        direct_followees, two_hop_identities
    ))
}
