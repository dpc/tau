//! Generic standalone replacements reuse bounded artifact verification without
//! becoming a terminal publication reservation.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::artifact_worker::ArtifactWorker;
use crate::image_verification::{
    ImageVerificationCompleted, ImageVerificationOwner, ImageVerificationRequest,
    ImageVerificationTerminal,
};

/// Logical owners remain revocable independently of physical worker permits.
#[derive(Default)]
pub(crate) struct CompactionImageAdmissions {
    /// One immutable provider terminal per compact prompt.
    pending: HashMap<AgentPromptId, PendingCompactionImages>,
}

/// Captured authority is rechecked before entering ordinary terminal reduction.
struct PendingCompactionImages {
    /// Exact session, source incarnation, runtime and persistence generation.
    owner: ImageVerificationOwner,
    /// Sticky cancellation flag and unique job identity.
    live: Arc<AtomicBool>,
    /// Original transaction tuple; output is omitted and suffix may advance.
    boundary: tau_proto::AgentCompacted,
    /// Retry accounting must not advance while the terminal is being verified.
    retry_attempt: u32,
    /// Exact terminal remains with its runtime owner, including known billing
    /// when disconnect races physical verification.
    response: Box<ProviderResponseFinished>,
}

impl CompactionImageAdmissions {
    /// Suppresses late provider reports while this terminal owns verification.
    pub(super) fn contains(&self, prompt: &AgentPromptId) -> bool {
        self.pending.contains_key(prompt)
    }

    /// Revokes physical work while leaving its terminal available for
    /// accounting.
    pub(super) fn cancel(&self, prompt: &AgentPromptId) {
        if let Some(pending) = self.pending.get(prompt) {
            pending.live.store(false, Ordering::Release);
        }
    }

    /// Retires work when ordinary prompt tracking is discarded or superseded.
    pub(super) fn remove(&mut self, prompt: &AgentPromptId) {
        self.cancel(prompt);
        self.pending.remove(prompt);
    }

    /// Session teardown revokes every queued or active storage operation.
    pub(super) fn revoke_all(&self) {
        for pending in self.pending.values() {
            pending.live.store(false, Ordering::Release);
        }
    }
}

impl Drop for CompactionImageAdmissions {
    fn drop(&mut self) {
        self.revoke_all();
    }
}

impl Harness {
    /// Checks the complete generic window and quota before sharing worker
    /// capacity. No provider policy requires equality to historical images.
    pub(super) fn begin_compaction_image_admission(
        &mut self,
        cid: &AgentId,
        response: &ProviderResponseFinished,
    ) -> Result<(), tau_proto::ArtifactError> {
        use tau_proto::ArtifactError;
        if !self.standalone_compaction_response_matches_current_branch(cid, response) {
            return Err(ArtifactError::Invalid);
        }
        let ProviderTerminalPlan::StandaloneCompaction(StandaloneCompactionTerminalPlan::Accepted(
            window,
        )) = self.classify_standalone_compaction_terminal(cid, response)
        else {
            return Err(ArtifactError::Invalid);
        };
        let (_, _, Event::AgentCompacted(mut boundary)) = self
            .standalone_compaction_boundary(cid, response, &window)
            .ok_or(ArtifactError::Invalid)?
        else {
            return Err(ArtifactError::Invalid);
        };
        boundary.replacement_window.clear();
        let source = self
            .provider_runtime
            .pending_prompts
            .get(&response.agent_prompt_id)
            .ok_or(ArtifactError::Unavailable)?;
        let owner = self
            .image_verification_owner(cid, source)
            .ok_or(ArtifactError::Unavailable)?;
        let retry_attempt = self
            .prompt_coordination
            .standalone_accounting
            .highest_retry_attempt
            .get(&response.agent_prompt_id)
            .copied()
            .unwrap_or_default();
        let live = Arc::new(AtomicBool::new(true));
        let request = ImageVerificationRequest {
            owner: owner.clone(),
            terminal: ImageVerificationTerminal::Standalone {
                prompt_id: response.agent_prompt_id.clone(),
                references: response
                    .output_items
                    .iter()
                    .filter_map(|item| match item {
                        ContextItem::ToolResult(result) => Some(&result.provider_content),
                        _ => None,
                    })
                    .flatten()
                    .filter_map(|part| match part {
                        tau_proto::ToolResultContentPart::ImageReference(reference) => {
                            Some(reference.clone())
                        }
                        _ => None,
                    })
                    .collect(),
            },
            live: Arc::clone(&live),
        };
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
            .submit_image(request)?;
        self.prompt_coordination
            .compaction_runtime
            .images
            .pending
            .insert(
                response.agent_prompt_id.clone(),
                PendingCompactionImages {
                    owner,
                    live,
                    boundary,
                    retry_attempt,
                    response: Box::new(response.clone()),
                },
            );
        Ok(())
    }

    /// Returns verified output through the original terminal reducer, which
    /// rechecks cancellation, branch/cut, quotas and canonical publication.
    pub(super) fn complete_compaction_image_admission(
        &mut self,
        completed: ImageVerificationCompleted,
    ) {
        let ImageVerificationCompleted {
            request,
            outcome,
            _permit,
        } = completed;
        let ImageVerificationTerminal::Standalone { prompt_id, .. } = request.terminal else {
            unreachable!("standalone completion");
        };
        let prompt = &prompt_id;
        let pending = &mut self.prompt_coordination.compaction_runtime.images.pending;
        if !pending.get(prompt).is_some_and(|pending| {
            Arc::ptr_eq(&pending.live, &request.live) && pending.owner.same_as(&request.owner)
        }) {
            return;
        }
        let pending = pending.remove(prompt).expect("matched image job");
        let mut response = pending.response;
        let live = pending.live.swap(false, Ordering::AcqRel);
        if !self
            .image_verification_owner(&pending.owner.cid, &pending.owner.connection)
            .is_some_and(|current| current.same_as(&pending.owner))
        {
            return;
        }
        let cid = &pending.owner.cid;
        let transaction_matches = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .is_some_and(|agent| {
                matches!(&agent.dispatch.activation_dispatch,
                path_crate_agent::ActivationDispatchState::Running {
                    id, cut, model, compact_prompt_id, ..
                } if id == &pending.boundary.transaction_id
                    && cut == &pending.boundary.cut
                    && model == &pending.boundary.model
                    && compact_prompt_id == prompt)
            });
        let attempt_matches = self
            .prompt_coordination
            .standalone_accounting
            .highest_retry_attempt
            .get(prompt)
            .copied()
            .unwrap_or_default()
            == pending.retry_attempt;
        let canceled = self.prompt_coordination.canceled_prompts.contains(prompt);
        let source = self.provider_runtime.pending_prompts.get(prompt);
        let source_matches =
            source == Some(&pending.owner.connection) || (canceled && source.is_none());
        if !attempt_matches || !source_matches {
            return;
        }
        if !transaction_matches && !canceled {
            // A successor transaction must not be failed by its predecessor's
            // terminal. The original dispatched attempt still owns its bill.
            reject_compaction_images(&mut response);
            self.account_early_standalone_terminal(
                cid,
                &mut response,
                Some(harness_connection_id()),
            );
            return;
        }
        // Cancellation still runs through the existing accounting correction
        // path. No rejected or revoked job can introduce its reference payload.
        if outcome.is_err() || !live || !transaction_matches {
            reject_compaction_images(&mut response);
        }
        if let Err(error) = self.handle_provider_response_finished_with_verified_images(*response) {
            self.runtime_io
                .publication
                .pending_error
                .get_or_insert(error);
        }
    }

    /// Retires one disconnected source, or all jobs before shutdown revokes
    /// session authority. Already reported billing must precede ordinary
    /// lifecycle closure as Unknown.
    pub(super) fn retire_compaction_image_admissions(
        &mut self,
        connection: Option<&tau_proto::ConnectionId>,
    ) {
        let prompts: Vec<_> = self
            .prompt_coordination
            .compaction_runtime
            .images
            .pending
            .iter()
            .filter(|(_, pending)| connection.is_none_or(|id| &pending.owner.connection == id))
            .map(|(prompt, _)| prompt.clone())
            .collect();
        for prompt in prompts {
            let pending = self
                .prompt_coordination
                .compaction_runtime
                .images
                .pending
                .remove(&prompt)
                .expect("collected image admission");
            pending.live.store(false, Ordering::Release);
            if self
                .image_verification_owner(&pending.owner.cid, &pending.owner.connection)
                .is_some_and(|current| current.same_as(&pending.owner))
            {
                let mut response = pending.response;
                reject_compaction_images(&mut response);
                self.account_early_standalone_terminal(
                    &pending.owner.cid,
                    &mut response,
                    Some(harness_connection_id()),
                );
            }
        }
    }
}

/// Errors contain neither provider-controlled text nor fetchable artifact keys.
pub(super) fn reject_compaction_images(response: &mut ProviderResponseFinished) {
    response.output_items.clear();
    response.stop_reason = tau_proto::ProviderStopReason::Error;
    response.failure_kind = Some(tau_proto::ProviderFailureKind::RequestRejected);
    response.error = Some("prepared image replacement could not be verified".to_owned());
}
