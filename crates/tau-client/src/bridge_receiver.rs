//! Shared opt-in configuration and nonblocking bridge resolution transport.
//!
//! Bridges retain ownership of admission, bounded retries, serialized snapshot
//! writes, designation generations, legacy import, and outstanding message
//! ACKs.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use tau_proto::{
    AgentId, BridgeReceiverMode, BridgeReceiverRequest, HarnessInputMessage, SessionId,
};

use crate::{ClientError, ClientHandle};

/// Per-instance receive policy; defaults never select or create automatically.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeReceiverConfig {
    /// Select automatically after startup and create only after admitted input.
    #[serde(default)]
    pub register_on_start: bool,
    /// Optional immutable creation-role constraint and lazy creation role.
    #[serde(default)]
    pub role: Option<String>,
}

impl BridgeReceiverConfig {
    /// Derive resolution authority without guessing a missing creation role.
    #[must_use]
    pub fn resolution_mode(
        &self,
        preferred_agent_id: Option<AgentId>,
        admitted_input: bool,
    ) -> BridgeReceiverMode {
        if !self.register_on_start {
            BridgeReceiverMode::Restore {
                agent_id: preferred_agent_id,
            }
        } else if admitted_input && self.role.is_some() {
            BridgeReceiverMode::Ensure { preferred_agent_id }
        } else {
            BridgeReceiverMode::Select { preferred_agent_id }
        }
    }
}

/// Session/instance-scoped designation snapshot, written only after selection.
///
/// Store with existing Session ExtensionData atomic replacement. Persist before
/// acknowledging register and serialize writes with designation generation
/// checks. In an ephemeral session keep this value in memory only. A failed
/// resolution must not erase it; a version other than one must fail closed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeReceiverSnapshot {
    /// Snapshot schema version; currently exactly one.
    pub version: u32,
    /// Last successfully designated identity, not a permission to force-load
    /// it.
    pub agent_id: AgentId,
}

impl BridgeReceiverSnapshot {
    /// Construct the current snapshot schema for one successful designation.
    #[must_use]
    pub fn new(agent_id: AgentId) -> Self {
        Self {
            version: 1,
            agent_id,
        }
    }

    /// Reject future/unknown layouts rather than silently importing legacy
    /// state.
    #[must_use]
    pub fn is_supported(&self) -> bool {
        self.version == 1
    }
}

/// Nonblocking sender; the ordinary manual receive loop owns correlated
/// results.
#[derive(Clone)]
pub struct BridgeReceiverClient {
    /// Existing configured runtime writer and its bounded detached queue.
    handle: ClientHandle,
}

impl BridgeReceiverClient {
    /// Attach to an already configured runtime; this performs no I/O.
    #[must_use]
    pub fn new(handle: ClientHandle) -> Self {
        Self { handle }
    }

    /// Send after Ready and return the correlation to match in the normal loop.
    ///
    /// Only `Ensure` permits creation, and only after actual external
    /// admission. A result timeout is uncertain: retry resolution, not
    /// external input publication. Generation-check a selected result
    /// before saving designation.
    pub fn start_request(
        &self,
        session_id: SessionId,
        role: Option<String>,
        mode: BridgeReceiverMode,
    ) -> Result<String, ClientError> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let request_id = format!("bridge-{}", NEXT.fetch_add(1, Ordering::Relaxed));
        self.handle
            .send_detached(HarnessInputMessage::BridgeReceiverRequest(
                BridgeReceiverRequest {
                    request_id: request_id.clone(),
                    session_id,
                    role,
                    mode,
                },
            ))?;
        Ok(request_id)
    }
}

#[cfg(test)]
mod tests;
