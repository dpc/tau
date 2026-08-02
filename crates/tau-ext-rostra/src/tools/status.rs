//! Local status tool.

use std::collections::HashSet;

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
    let followees = client.db().get_followees(identity).await;
    let direct_ids: HashSet<_> = followees.iter().map(|(id, _)| *id).collect();
    let mut two_hop = HashSet::new();
    for direct_id in &direct_ids {
        for (id, _) in client.db().get_followees(*direct_id).await {
            if id != identity && !direct_ids.contains(&id) {
                two_hop.insert(id);
            }
        }
    }
    bounded_output(format!(
        "identity: {identity}\nmode: read-only\ntransport: relay-only Iroh peer transport; Pkarr HTTPS/DNS discovery; no direct peer-IP\ndatabase: open\nknown_direct_followees: {}\nknown_two_hop_identities: {}\nclient_state: started\nsynchronization_health: unknown",
        direct_ids.len(),
        two_hop.len()
    ))
}
