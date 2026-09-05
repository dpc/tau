//! Attachment-local selection, creation, and editor-recovery authority.

/// Explicit input-routing mode owned by one terminal attachment.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) enum UiTarget {
    /// Untouched startup overview that attach completion may claim once.
    #[default]
    InitialOverview,
    /// Explicit all-agent overview; prompt submission is disabled.
    Overview,
    /// Existing loaded agent targeted by prompt input.
    Viewing(tau_proto::AgentId),
    /// Explicit new-agent composer entered by `:agent new` or `:new`.
    Creating,
}

/// Target resolved from exhaustive attach-time session state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum InitialAttachTarget {
    /// No replayable or current runtime agent exists, so the first prompt may
    /// create one without an explicit command.
    FreshSession,
    /// Existing state is absent from the unique active intersection or was too
    /// ambiguous to select safely.
    Overview,
    /// Sole replayable agent present in the current runtime.
    Agent(Box<tau_proto::AgentId>),
}

/// Semantic no-agent targets accepted by renderer commands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EmptyUiTarget {
    /// Non-interactive all-agent overview.
    Overview,
    /// Explicit new-agent composer.
    Creating,
}

impl From<EmptyUiTarget> for UiTarget {
    fn from(target: EmptyUiTarget) -> Self {
        match target {
            EmptyUiTarget::Overview => Self::Overview,
            EmptyUiTarget::Creating => Self::Creating,
        }
    }
}

/// Pending explicit create request owned by one terminal attachment.
#[derive(Clone, Debug, PartialEq)]
struct PendingUiCreate {
    /// Exact request sent to the harness, retained for correlation and
    /// recovery.
    request: tau_proto::UiCreateAgent,
    /// Intent epoch that owned the create submission.
    submitted_epoch: u64,
    /// Terminal editor revision immediately after the submitted prompt cleared.
    editor_revision: u64,
}

/// Initial prompt retained until its durable submission or correlated failure.
#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingInitialPrompt {
    /// Created agent that owns the initial prompt.
    agent_id: tau_proto::AgentId,
    /// Correlation copied into the prompt lifecycle.
    ctx_id: String,
    /// Create request correlation used by prompt-failure events.
    request_id: String,
    /// Submitted text retained for local recovery only.
    text: String,
    /// Exact selected-agent epoch that may still restore the text.
    owned_epoch: u64,
    /// Editor revision inherited from the create submission.
    editor_revision: u64,
}

/// Client-local effect claimed from one matching create result.
pub(super) enum UiCreateResultEffect {
    /// The result did not own the current attachment intent.
    None,
    /// Select the created agent.
    Select {
        /// Created agent identity.
        agent_id: tau_proto::AgentId,
        /// Claimed intent epoch.
        intent_epoch: u64,
    },
    /// Restore rejected text into the still-owned composer.
    RestoreDraft {
        /// Rejected submission text.
        text: String,
        /// Claimed intent epoch.
        intent_epoch: u64,
        /// Claimed semantic target.
        target: UiTarget,
        /// Editor revision captured at submission.
        editor_revision: u64,
    },
    /// Select the committed agent and restore its rejected initial prompt.
    SelectAndRestore {
        /// Committed agent identity.
        agent_id: tau_proto::AgentId,
        /// Claimed intent epoch.
        intent_epoch: u64,
        /// Rejected initial prompt.
        text: String,
        /// Editor revision captured at submission.
        editor_revision: u64,
    },
}

/// Editor restoration claimed from a matching initial-prompt failure.
pub(super) struct DraftRecovery {
    /// Text eligible for restoration.
    pub(super) text: String,
    /// Intent epoch that owns the restoration.
    pub(super) intent_epoch: u64,
    /// Semantic target that owns the restoration.
    pub(super) target: UiTarget,
    /// Editor revision captured at submission.
    pub(super) editor_revision: u64,
}

/// Current input target and requester-directed create ownership.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SelectionIntent {
    /// Monotonic local-selection claim epoch.
    epoch: u64,
    /// Explicit input target mode.
    target: UiTarget,
    /// At most one create request awaiting its requester-directed result.
    pending_create: Option<PendingUiCreate>,
    /// Initial create prompt awaiting durable submission or failure.
    pending_initial_prompt: Option<PendingInitialPrompt>,
    /// Current editable prompt buffer owned only by this attachment.
    editable_draft: String,
}

impl SelectionIntent {
    /// Returns the selected agent as a borrowed string-like identifier.
    #[cfg(test)]
    pub(crate) fn as_ref(&self) -> Option<&tau_proto::AgentId> {
        self.selected_agent_id()
    }

    /// Returns the selected agent's validated text.
    #[cfg(test)]
    pub(crate) fn as_deref(&self) -> Option<&str> {
        self.selected_agent_id().map(tau_proto::AgentId::as_str)
    }

    /// Returns the exact existing agent targeted by prompt input.
    pub(crate) fn selected_agent_id(&self) -> Option<&tau_proto::AgentId> {
        match &self.target {
            UiTarget::Viewing(agent_id) => Some(agent_id),
            UiTarget::InitialOverview | UiTarget::Overview | UiTarget::Creating => None,
        }
    }

    /// Returns whether prompt text is allowed to create a new agent.
    pub(crate) fn is_creating(&self) -> bool {
        matches!(self.target, UiTarget::Creating)
    }

    /// Returns the current semantic target.
    pub(crate) fn target(&self) -> &UiTarget {
        &self.target
    }

    /// Returns the current monotonic intent epoch.
    #[cfg(test)]
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Returns whether a create request is already pending.
    pub(crate) fn has_pending_create(&self) -> bool {
        self.pending_create.is_some()
    }

    /// Advances to one explicit target and returns its claimed epoch.
    pub(crate) fn set_target(&mut self, target: UiTarget) -> u64 {
        self.epoch = self.epoch.saturating_add(1);
        self.target = target;
        self.epoch
    }

    /// Returns whether one epoch and target still own this intent.
    pub(super) fn matches(&self, epoch: u64, target: &UiTarget) -> bool {
        self.epoch == epoch && &self.target == target
    }

    /// Claims untouched startup as explicit overview.
    pub(super) fn claim_initial_overview(&mut self) -> bool {
        if self.epoch != 0 || !matches!(self.target, UiTarget::InitialOverview) {
            return false;
        }
        self.epoch = 1;
        self.target = UiTarget::Overview;
        true
    }

    /// Claims untouched startup as the implicit fresh-session composer.
    pub(super) fn claim_initial_creation(&mut self) -> bool {
        if self.epoch != 0 || !matches!(self.target, UiTarget::InitialOverview) {
            return false;
        }
        self.epoch = 1;
        self.target = UiTarget::Creating;
        true
    }

    /// Claims untouched startup for one validated existing agent.
    pub(super) fn claim_initial_agent(&mut self, agent_id: tau_proto::AgentId) -> Option<u64> {
        if self.epoch != 0 || !matches!(self.target, UiTarget::InitialOverview) {
            return None;
        }
        self.epoch = 1;
        self.target = UiTarget::Viewing(agent_id);
        Some(self.epoch)
    }

    /// Returns whether attach completion still owns untouched startup.
    pub(super) fn is_initial_overview(&self) -> bool {
        matches!(self.target, UiTarget::InitialOverview)
    }

    /// Stages exactly one explicit create request.
    pub(crate) fn stage_create(
        &mut self,
        request: tau_proto::UiCreateAgent,
        editor_revision: u64,
    ) -> Result<(), &'static str> {
        if !self.is_creating() {
            return Err("Use :agent new before creating an agent.");
        }
        if self.pending_create.is_some() {
            return Err("Agent creation is already pending.");
        }
        self.editable_draft.clear();
        self.pending_create = Some(PendingUiCreate {
            submitted_epoch: self.epoch,
            request,
            editor_revision,
        });
        Ok(())
    }

    /// Clears one matching staged create after local send failure.
    pub(crate) fn clear_staged_create(&mut self, request_id: &str) {
        if self
            .pending_create
            .as_ref()
            .is_some_and(|pending| pending.request.request_id == request_id)
        {
            self.pending_create = None;
        }
    }

    /// Records the newest editable draft and invalidates pending recovery when
    /// its text changed.
    pub(crate) fn record_editable_draft(&mut self, text: &str) {
        if (self.pending_create.is_some() || self.pending_initial_prompt.is_some())
            && self.editable_draft != text
        {
            self.epoch = self.epoch.saturating_add(1);
        }
        text.clone_into(&mut self.editable_draft);
    }

    /// Claims one matching requester-directed create result.
    pub(super) fn claim_create_result(
        &mut self,
        result: &tau_proto::UiCreateAgentResult,
    ) -> UiCreateResultEffect {
        let Some(pending) = self.pending_create.take() else {
            return UiCreateResultEffect::None;
        };
        if pending.request.request_id != result.request_id
            || pending.request.session_id != result.session_id
        {
            self.pending_create = Some(pending);
            return UiCreateResultEffect::None;
        }
        if self.epoch != pending.submitted_epoch || !matches!(self.target, UiTarget::Creating) {
            return UiCreateResultEffect::None;
        }
        let submitted_text = pending.request.initial_prompt.unwrap_or_default();
        match &result.outcome {
            tau_proto::UiCreateAgentOutcome::Created { agent_id, .. } => {
                let epoch = self.set_target(UiTarget::Viewing(agent_id.clone()));
                if let Some(ctx_id) = pending.request.ctx_id {
                    self.pending_initial_prompt = Some(PendingInitialPrompt {
                        agent_id: agent_id.clone(),
                        ctx_id,
                        request_id: pending.request.request_id,
                        text: submitted_text,
                        owned_epoch: epoch,
                        editor_revision: pending.editor_revision,
                    });
                }
                UiCreateResultEffect::Select {
                    agent_id: agent_id.clone(),
                    intent_epoch: epoch,
                }
            }
            tau_proto::UiCreateAgentOutcome::Rejected {
                agent_id: Some(agent_id),
                ..
            } => {
                let epoch = self.set_target(UiTarget::Viewing(agent_id.clone()));
                submitted_text.clone_into(&mut self.editable_draft);
                UiCreateResultEffect::SelectAndRestore {
                    agent_id: agent_id.clone(),
                    intent_epoch: epoch,
                    text: submitted_text,
                    editor_revision: pending.editor_revision,
                }
            }
            tau_proto::UiCreateAgentOutcome::Rejected { agent_id: None, .. } => {
                submitted_text.clone_into(&mut self.editable_draft);
                UiCreateResultEffect::RestoreDraft {
                    text: submitted_text,
                    intent_epoch: self.epoch,
                    target: self.target.clone(),
                    editor_revision: pending.editor_revision,
                }
            }
        }
    }

    /// Claims a matching durable initial-prompt lifecycle.
    pub(super) fn claim_initial_prompt_lifecycle(
        &mut self,
        event: &tau_proto::Event,
    ) -> Option<DraftRecovery> {
        match event {
            tau_proto::Event::AgentPromptSubmitted(submitted) => {
                let matches = self.pending_initial_prompt.as_ref().is_some_and(|pending| {
                    submitted.ctx_id.as_deref() == Some(pending.ctx_id.as_str())
                        && submitted.agent_id == pending.agent_id
                });
                if matches {
                    self.pending_initial_prompt = None;
                }
                None
            }
            tau_proto::Event::AgentPromptFailed(failed) => {
                let matches = self.pending_initial_prompt.as_ref().is_some_and(|pending| {
                    failed.ctx_id == pending.ctx_id && failed.request_id == pending.request_id
                });
                if !matches {
                    return None;
                }
                let pending = self
                    .pending_initial_prompt
                    .take()
                    .expect("matching initial prompt recovery exists");
                if self.epoch != pending.owned_epoch
                    || self.selected_agent_id() != Some(&pending.agent_id)
                {
                    return None;
                }
                pending.text.clone_into(&mut self.editable_draft);
                Some(DraftRecovery {
                    text: pending.text,
                    intent_epoch: pending.owned_epoch,
                    target: UiTarget::Viewing(pending.agent_id),
                    editor_revision: pending.editor_revision,
                })
            }
            _ => None,
        }
    }

    /// Returns the current editable draft for focused assertions.
    #[cfg(test)]
    pub(crate) fn editable_draft(&self) -> &str {
        &self.editable_draft
    }

    /// Returns whether initial-prompt recovery remains pending.
    #[cfg(test)]
    pub(crate) fn has_pending_initial_prompt(&self) -> bool {
        self.pending_initial_prompt.is_some()
    }

    /// Builds a valid explicit-creation state with one pending request.
    #[cfg(test)]
    pub(crate) fn test_creating(
        epoch: u64,
        request: tau_proto::UiCreateAgent,
        submitted_epoch: u64,
        editor_revision: u64,
    ) -> Self {
        Self {
            epoch,
            target: UiTarget::Creating,
            pending_create: Some(PendingUiCreate {
                request,
                submitted_epoch,
                editor_revision,
            }),
            pending_initial_prompt: None,
            editable_draft: String::new(),
        }
    }

    /// Builds a valid viewing state retaining one stale create result.
    #[cfg(test)]
    pub(crate) fn test_viewing_with_pending_create(
        epoch: u64,
        agent_id: tau_proto::AgentId,
        editable_draft: impl Into<String>,
        request: tau_proto::UiCreateAgent,
        submitted_epoch: u64,
        editor_revision: u64,
    ) -> Self {
        Self {
            epoch,
            target: UiTarget::Viewing(agent_id),
            pending_create: Some(PendingUiCreate {
                request,
                submitted_epoch,
                editor_revision,
            }),
            pending_initial_prompt: None,
            editable_draft: editable_draft.into(),
        }
    }

    /// Builds a valid viewing state with correlated initial-prompt recovery.
    #[cfg(test)]
    pub(crate) fn test_viewing_with_initial_prompt(
        epoch: u64,
        agent_id: tau_proto::AgentId,
        ctx_id: String,
        request_id: String,
        text: String,
        editor_revision: u64,
    ) -> Self {
        Self {
            epoch,
            target: UiTarget::Viewing(agent_id.clone()),
            pending_create: None,
            pending_initial_prompt: Some(PendingInitialPrompt {
                agent_id,
                ctx_id,
                request_id,
                text,
                owned_epoch: epoch,
                editor_revision,
            }),
            editable_draft: String::new(),
        }
    }

    /// Builds a valid selected-agent state at an exact test epoch.
    #[cfg(test)]
    pub(crate) fn test_viewing(epoch: u64, agent_id: tau_proto::AgentId) -> Self {
        Self {
            epoch,
            target: UiTarget::Viewing(agent_id),
            ..Self::default()
        }
    }

    /// Claims a create result and returns the propagated recovery revision.
    #[cfg(test)]
    pub(crate) fn test_claim_create_recovery_revision(
        &mut self,
        result: &tau_proto::UiCreateAgentResult,
    ) -> Option<u64> {
        match self.claim_create_result(result) {
            UiCreateResultEffect::RestoreDraft {
                editor_revision, ..
            }
            | UiCreateResultEffect::SelectAndRestore {
                editor_revision, ..
            } => Some(editor_revision),
            UiCreateResultEffect::None | UiCreateResultEffect::Select { .. } => None,
        }
    }

    /// Claims an initial-prompt failure and returns its propagated editor
    /// revision.
    #[cfg(test)]
    pub(crate) fn test_claim_initial_failure_revision(
        &mut self,
        event: &tau_proto::Event,
    ) -> Option<u64> {
        self.claim_initial_prompt_lifecycle(event)
            .map(|recovery| recovery.editor_revision)
    }
}
