//! Tool-result image admission remains pending runtime work until verified
//! bytes and durable roots permit the ordinary canonical terminal path.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::artifact_worker::ArtifactWorker;
use crate::image_verification::{
    ImageVerificationCompleted, ImageVerificationOwner, ImageVerificationRequest,
    ImageVerificationTerminal,
};

/// Logical ownership is separate from the worker's queued/physical/completion
/// permit.
#[derive(Default)]
pub(crate) struct ImageAdmissionRuntime {
    /// At most one verification or settling terminal per existing tool call.
    pending: HashMap<ToolCallId, PendingImageAdmission>,
}

/// Captured authority survives I/O but never becomes a semantic reservation.
struct PendingImageAdmission {
    /// Exact original call/source/runtime/lease ownership.
    owner: ImageVerificationOwner,
    /// Per-operation identity, revoked independently of connection lifetime.
    live: Arc<AtomicBool>,
    /// A competing error/cancellation already owns terminal settlement.
    settling: bool,
}

impl ImageAdmissionRuntime {
    /// Prevents duplicate producer reports from creating more verification
    /// work.
    pub(super) fn contains(&self, call_id: &str) -> bool {
        self.pending.contains_key(call_id)
    }

    /// Identifies a terminal already submitted by verification cancellation.
    pub(super) fn is_settling(&self, call_id: &str) -> bool {
        self.pending
            .get(call_id)
            .is_some_and(|pending| pending.settling)
    }

    /// Revokes work without publishing or changing ordinary call settlement.
    pub(super) fn cancel(&mut self, call_id: &str) -> Option<ImageVerificationOwner> {
        let pending = self.pending.get_mut(call_id)?;
        pending.live.store(false, Ordering::Release);
        if pending.settling {
            return None;
        }
        pending.settling = true;
        Some(pending.owner.clone())
    }

    /// Retires exact call authority only when its normal runtime tracking ends.
    pub(super) fn remove(&mut self, call_id: &str) {
        if let Some(pending) = self.pending.remove(call_id) {
            pending.live.store(false, Ordering::Release);
        }
    }

    /// Disconnect revokes storage work before lifecycle synthesis can run.
    pub(super) fn disconnect(&mut self, connection: &tau_proto::ConnectionId) {
        for pending in self.pending.values_mut() {
            if &pending.owner.connection == connection {
                pending.live.store(false, Ordering::Release);
            }
        }
    }

    /// Revokes every physical job before session teardown releases writer
    /// leases.
    pub(super) fn revoke_all(&mut self) {
        for pending in self.pending.values_mut() {
            pending.live.store(false, Ordering::Release);
        }
    }
}

impl Drop for ImageAdmissionRuntime {
    fn drop(&mut self) {
        for pending in self.pending.values() {
            pending.live.store(false, Ordering::Release);
        }
    }
}

impl Harness {
    /// Captures prepared-image storage before interception. Only a persistent
    /// session/agent with live scoped I/O authority may require durable refs.
    pub(super) fn prepared_image_storage_for_tool(
        &self,
        cid: &AgentId,
        target: &ToolRouteTarget,
        allows_provider_image: bool,
    ) -> tau_proto::PreparedImageStorage {
        if allows_provider_image
            && let ToolRouteTarget::Extension(source) = target
            && self.image_verification_owner(cid, source).is_some()
        {
            tau_proto::PreparedImageStorage::Artifact
        } else {
            tau_proto::PreparedImageStorage::Inline
        }
    }

    /// Captures metadata-only authority before worker admission, without CAS
    /// I/O.
    pub(super) fn image_verification_owner(
        &self,
        cid: &AgentId,
        source: &tau_proto::ConnectionId,
    ) -> Option<ImageVerificationOwner> {
        if !self.session_runtime.storage_mode.is_durable()
            || self.session_runtime.shutdown_published
        {
            return None;
        }
        let agent = self.agent_runtime.agent_registry.agents.get(cid)?;
        if agent.identity.persistence.is_ephemeral()
            || agent.identity.session_id != self.session_runtime.current_session_id
        {
            return None;
        }
        let agent_id = agent.identity.agent_id.as_ref()?;
        let lease = self
            .session_runtime
            .agent_store
            .managed_persistence_lease(agent_id)?;
        lease.validate_agent_io_authority().ok()?;
        let entry = self.extensions.entries.get(source)?;
        if entry.state == ExtensionState::Disconnected
            || !matches!(
                entry.kind,
                ClientKind::Tool | ClientKind::Core | ClientKind::Provider
            )
        {
            return None;
        }
        Some(ImageVerificationOwner {
            cid: cid.clone(),
            runtime_incarnation: agent.identity.runtime_incarnation,
            session_id: self.session_runtime.current_session_id.clone(),
            session_generation: self.session_runtime.current_session_generation,
            connection: source.clone(),
            instance_id: entry.instance_id,
            lease,
        })
    }

    /// Revalidates the exact call and generation; ordinary head movement is
    /// legal because core completes pending rounds on their original
    /// branches.
    fn image_verification_owner_is_current(
        &self,
        call_id: &ToolCallId,
        owner: &ImageVerificationOwner,
    ) -> bool {
        if self.tool_routing.tool_runtime.tool_agents.get(call_id) != Some(&owner.cid)
            || self
                .tool_routing
                .tool_runtime
                .pending_tool_providers
                .get(call_id)
                != Some(&owner.connection)
            || self
                .tool_routing
                .tool_runtime
                .tool_turn
                .is_backgrounded(call_id)
            || !self
                .tool_routing
                .tool_runtime
                .pending_tools
                .get(call_id)
                .is_some_and(|tool| {
                    tool.allows_provider_image
                        && tool.image_storage == tau_proto::PreparedImageStorage::Artifact
                })
        {
            return false;
        }
        self.image_verification_owner(&owner.cid, &owner.connection)
            .is_some_and(|current| current.same_as(owner))
    }

    /// Starts bounded verification only after source, policy, shape, quota and
    /// call-causality checks passed through the normal intake path.
    pub(super) fn begin_tool_image_admission(
        &mut self,
        cid: &AgentId,
        source: &tau_proto::ConnectionId,
        result: ToolResult,
    ) {
        if self
            .tool_routing
            .tool_runtime
            .images
            .contains(&result.call_id)
            || self
                .tool_routing
                .tool_runtime
                .pending_terminal_observations
                .contains_key(&result.call_id)
        {
            return;
        }
        let mut failure = image_admission_error(&result, "durable image admission is unavailable");
        let Some(owner) = self.image_verification_owner(cid, source) else {
            self.publish_terminal_tool_error(Some(cid), Some(harness_connection_id()), failure);
            return;
        };
        let call_id = result.call_id.clone();
        let live = Arc::new(AtomicBool::new(true));
        let request = ImageVerificationRequest {
            owner: owner.clone(),
            terminal: ImageVerificationTerminal::Tool(Box::new(result)),
            live: Arc::clone(&live),
        };
        let admitted = (|| {
            if self.runtime_io.artifacts.is_none() {
                self.runtime_io.artifacts = Some(ArtifactWorker::start_with_limit(
                    &self.session_runtime.state_dir,
                    self.runtime_io.tx.clone(),
                    self.config
                        .accepted_harness_settings
                        .artifact_limits
                        .stored_bytes,
                )?);
            }
            self.runtime_io
                .artifacts
                .as_mut()
                .expect("worker initialized")
                .submit_image(request)
        })();
        match admitted {
            Ok(()) => {
                self.tool_routing
                    .tool_runtime
                    .tool_turn
                    .clear_background_deadline(&call_id);
                self.tool_routing.tool_runtime.images.pending.insert(
                    call_id,
                    PendingImageAdmission {
                        owner,
                        live,
                        settling: false,
                    },
                );
            }
            Err(error) => {
                live.store(false, Ordering::Release);
                failure.message = image_admission_failure_message(error).to_owned();
                self.publish_terminal_tool_error(Some(cid), Some(harness_connection_id()), failure);
            }
        }
    }

    /// Physical custody ended before this completion entered the main-loop
    /// queue.
    pub(super) fn complete_image_admission(&mut self, completed: ImageVerificationCompleted) {
        if matches!(
            completed.request.terminal,
            ImageVerificationTerminal::Standalone { .. }
        ) {
            self.complete_compaction_image_admission(completed);
            return;
        }
        let ImageVerificationCompleted {
            request,
            outcome,
            _permit,
        } = completed;
        let ImageVerificationTerminal::Tool(terminal) = request.terminal else {
            unreachable!("standalone completion routed above");
        };
        let call_id = terminal.call_id.clone();
        let matches = self
            .tool_routing
            .tool_runtime
            .images
            .pending
            .get(&call_id)
            .is_some_and(|pending| {
                Arc::ptr_eq(&pending.live, &request.live) && pending.owner.same_as(&request.owner)
            });
        if !matches {
            return;
        }
        let pending = self
            .tool_routing
            .tool_runtime
            .images
            .pending
            .remove(&call_id)
            .expect("matched verification");
        let live = pending.live.swap(false, Ordering::AcqRel);
        if !live
            || pending.settling
            || !self.image_verification_owner_is_current(&call_id, &pending.owner)
        {
            return;
        }
        let cid = pending.owner.cid;
        let validation = self.validate_owned_tool_result(&cid, &terminal);
        match (outcome, validation) {
            (Ok(()), Ok(())) => {
                let mut result = *terminal;
                self.dedup_tool_result(&cid, &mut result);
                self.publish_terminal_tool_result(
                    Some(&cid),
                    Some(harness_connection_id()),
                    result,
                );
            }
            (Err(error), _) => {
                let error =
                    image_admission_error(&terminal, image_admission_failure_message(error));
                self.publish_terminal_tool_error(Some(&cid), Some(harness_connection_id()), error);
            }
            (_, Err(_)) => {
                let error = image_admission_error(
                    &terminal,
                    "image result rejected by transcript safety validation",
                );
                self.publish_terminal_tool_error(Some(&cid), Some(harness_connection_id()), error);
            }
        }
    }

    /// A committed cancel request must settle this harness-owned phase: the
    /// producer already reported completion and has no remaining work to
    /// cancel.
    pub(super) fn cancel_tool_image_admission(&mut self, call_id: &ToolCallId) {
        let Some(owner) = self.tool_routing.tool_runtime.images.cancel(call_id) else {
            return;
        };
        if !self.image_verification_owner_is_current(call_id, &owner) {
            self.tool_routing.tool_runtime.images.remove(call_id);
            return;
        }
        let tool = self
            .tool_routing
            .tool_runtime
            .pending_tools
            .get(call_id)
            .expect("current call");
        let cancelled = ToolCancelled {
            output: None,
            captured_output: None,
            presentation: Default::default(),
            call_id: call_id.clone(),
            tool_name: tool.name.clone(),
            tool_type: tool.tool_type,
            display: None,
        };
        let cause = self
            .tool_routing
            .tool_runtime
            .pending_cancellation_observations
            .get(call_id)
            .copied()
            .map_or(tau_proto::ToolTerminalCause::Unknown, |request| {
                tau_proto::ToolTerminalCause::Cancellation { request }
            });
        self.observe_tool_terminal(&owner.cid, call_id, cause);
        self.publish_for_agent_from(
            &owner.cid,
            Some(harness_connection_id()),
            Event::ToolCancelled(cancelled),
        );
    }

    /// Reuses ordinary causal, logical-quota and current-head admission checks.
    pub(super) fn validate_owned_tool_result(
        &mut self,
        cid: &AgentId,
        result: &ToolResult,
    ) -> Result<(), String> {
        let agent = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .ok_or_else(|| "the owning agent is unavailable".to_owned())?;
        let agent_id = agent
            .identity
            .agent_id
            .as_ref()
            .ok_or_else(|| "the owning transcript is unavailable".to_owned())?;
        let parent = agent.identity.head.map_or(
            tau_core::AgentEventParent::Root,
            tau_core::AgentEventParent::Under,
        );
        self.session_runtime
            .agent_store
            .validate_agent_event_at(
                agent_id,
                Some(tau_core::PersistedEventSource::Connection(
                    harness_connection_id().clone(),
                )),
                parent,
                &Event::ProviderToolResult(result.clone()),
                tau_proto::UnixMicros::now(),
            )
            .map_err(|error| error.to_string())
    }
}

/// Produces a payload-free error without exposing prepared keys or storage
/// paths.
fn image_admission_error(result: &ToolResult, message: &str) -> ToolError {
    ToolError {
        captured_output: None,
        presentation: Default::default(),
        call_id: result.call_id.clone(),
        tool_name: result.tool_name.clone(),
        tool_type: result.tool_type,
        message: message.to_owned(),
        details: None,
        display: None,
        originator: result.originator.clone(),
    }
}

/// Closed categories preserve useful diagnostics without fetchable authority.
fn image_admission_failure_message(error: tau_proto::ArtifactError) -> &'static str {
    match error {
        tau_proto::ArtifactError::Busy => "prepared image storage is busy",
        tau_proto::ArtifactError::Integrity => "prepared image failed size or digest verification",
        tau_proto::ArtifactError::Invalid => {
            "prepared image media or retention metadata was rejected"
        }
        tau_proto::ArtifactError::Io => "prepared image storage I/O failed",
        tau_proto::ArtifactError::Unavailable
        | tau_proto::ArtifactError::Permission
        | tau_proto::ArtifactError::SessionMismatch => {
            "prepared image or durable transcript is unavailable"
        }
    }
}
