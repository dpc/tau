//! Owns enqueue-to-commit publication and canonical event persistence.
//!
//! The interception chain remains in `interception`; this module preserves the
//! governed commit ordering from enqueue through persistence and publication.

use super::*;

impl Harness {
    /// Agent id that owns a given in-flight prompt, if any.
    pub(super) fn agent_id_for_prompt(&self, spid: &AgentPromptId) -> Option<AgentId> {
        self.prompt_coordination
            .prompt_runtime
            .agents
            .get(spid)
            .cloned()
    }

    /// If the agent's dedup map's "built for" cursor doesn't
    /// match its current `head`, rebuild it from the assembled branch.
    /// O(branch_len) on rebuild; O(1) on the steady-state hot path
    /// where the linear-extension hook in [`Self::commit_event`] keeps
    /// `built_for` in sync after every fold.
    ///
    /// `None` is returned only if the conversation no longer exists
    /// (the caller raced its own teardown), and the caller treats that
    /// as "skip dedup, just publish".
    pub(super) fn ensure_dedup_built_for_branch(&mut self, cid: &AgentId) -> Option<()> {
        let head = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)?
            .identity
            .head;
        let needs = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .map(|c| c.execution.result_dedup.needs_rebuild(head))
            .unwrap_or(false);
        if !needs {
            return Some(());
        }
        // Walk the branch under an immutable borrow of the store, then
        // hand the snapshot to the conversation under a mut borrow —
        // the branch iterator borrows the tree, so we materialize it
        // into an owned Vec first to release the tree borrow.
        let agent_id = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)?
            .identity
            .agent_id
            .clone();
        let branch: Vec<tau_core::AgentEntry> = agent_id
            .as_deref()
            .and_then(|agent_id| self.session_runtime.agent_store.agent(agent_id))
            .map(|t| t.branch_from(head).into_iter().cloned().collect())
            .unwrap_or_default();
        let conv = self.agent_runtime.agent_registry.agents.get_mut(cid)?;
        conv.execution.result_dedup.rebuild_from_branch(
            branch.iter(),
            head,
            DEFAULT_THRESHOLD_BYTES,
        );
        Some(())
    }

    /// Replace `result.result` with a pointer if a previous tool
    /// result on this agent's branch has the same content.
    /// Mutates `result` in place; the caller publishes the (possibly
    /// modified) value, which is what gets folded into the tree and
    /// what the LLM sees on the next turn.
    pub(super) fn dedup_tool_result(&mut self, cid: &AgentId, result: &mut tau_proto::ToolResult) {
        if self.ensure_dedup_built_for_branch(cid).is_none() {
            return;
        }
        let bytes = encode_for_hash(&result.result);
        if bytes.len() < DEFAULT_THRESHOLD_BYTES {
            return;
        }
        let hash = hash_truncated(&bytes);
        let Some(conv) = self.agent_runtime.agent_registry.agents.get_mut(cid) else {
            return;
        };
        if let Some(original_call_id) = conv.execution.result_dedup.lookup(&hash).cloned() {
            // Belt-and-suspenders: refuse to point a call at itself.
            // This can't happen in practice — `tool_agents`
            // already drops the call_id between intake and now — but
            // a future change to the tracking map could let a tool
            // result re-enter this path twice, and self-pointing is a
            // worse failure mode than just skipping the dedup.
            if original_call_id == result.call_id {
                return;
            }
            tracing::debug!(
                target: "tau_harness",
                cid = %cid,
                tool = %result.tool_name,
                call_id = %result.call_id,
                points_to = %original_call_id,
                bytes = bytes.len(),
                "deduping tool result against earlier identical output"
            );
            result.result = build_pointer_value(&original_call_id, &result.tool_name);
            result.presentation = tau_proto::ToolResultPresentation::HarnessDedupPointer;
        } else {
            conv.execution
                .result_dedup
                .insert(hash, result.call_id.clone());
        }
    }

    /// Companion to [`Self::dedup_tool_result`] for `ToolError`s.
    /// Same semantics — collapses repeated identical errors (same
    /// message, same `details`) into a pointer back to the first
    /// occurrence on this branch.
    pub(super) fn dedup_tool_error(&mut self, cid: &AgentId, error: &mut tau_proto::ToolError) {
        if self.ensure_dedup_built_for_branch(cid).is_none() {
            return;
        }
        let bytes = encode_error_for_hash(&error.message, error.details.as_ref());
        if bytes.len() < DEFAULT_THRESHOLD_BYTES {
            return;
        }
        let hash = hash_truncated(&bytes);
        let Some(conv) = self.agent_runtime.agent_registry.agents.get_mut(cid) else {
            return;
        };
        if let Some(original_call_id) = conv.execution.result_dedup.lookup(&hash).cloned() {
            if original_call_id == error.call_id {
                return;
            }
            tracing::debug!(
                target: "tau_harness",
                cid = %cid,
                tool = %error.tool_name,
                call_id = %error.call_id,
                points_to = %original_call_id,
                bytes = bytes.len(),
                "deduping tool error against earlier identical output"
            );
            error.message = build_pointer_error_message(&original_call_id, &error.tool_name);
            error.details = None;
            error.presentation = tau_proto::ToolResultPresentation::HarnessDedupPointer;
        } else {
            conv.execution
                .result_dedup
                .insert(hash, error.call_id.clone());
        }
    }

    /// Publishes an event for a specific conversation. The fold uses
    /// the agent's `head` as the explicit parent — no more
    /// `UiNavigateTree` head-bouncing — and the post-commit hook in
    /// [`Harness::commit_event`] keeps `c.head` in sync with the
    /// freshly-folded node.
    ///
    /// This helper is what makes branching prompts work: a user
    /// conversation can keep advancing while a side agent from an
    /// extension grows its own branch off some earlier node;
    /// each side publish brackets its own navigate-then-append.
    pub(crate) fn publish_for_agent(&mut self, cid: &AgentId, event: Event) {
        self.publish_for_agent_from(cid, None, event);
    }

    /// Append a content-free runtime observation without waiting for stable
    /// storage or routing it through interception and subscriber delivery.
    ///
    /// The semantic append still validates and writes one failure-atomic
    /// journal frame synchronously. The caller must perform the runtime
    /// action regardless of that append's result; file-data and directory
    /// synchronization remain asynchronous. The return value reports only
    /// whether this immediate append succeeded.
    pub(crate) fn append_best_effort_observation(
        &mut self,
        cid: &AgentId,
        observation_id: tau_proto::ObservationId,
        event: Event,
    ) -> bool {
        let Some(agent) = self.agent_runtime.agent_registry.agents.get(cid) else {
            return false;
        };
        let Some(agent_id) = agent.identity.agent_id.as_deref() else {
            return false;
        };
        let parent = agent
            .identity
            .head
            .map(tau_core::AgentEventParent::Under)
            .unwrap_or(tau_core::AgentEventParent::Root);
        let result = self
            .session_runtime
            .agent_store
            .append_agent_event_at_with_observation_id(
                agent_id,
                None,
                parent,
                event,
                tau_proto::UnixMicros::now(),
                observation_id,
            );
        let succeeded = result.is_ok();
        if let Err(error) = result {
            tracing::warn!(
                target: "tau_harness",
                %error,
                "best-effort runtime observation append failed"
            );
        }
        succeeded
    }

    /// Append and time one activation observation with a caller-provided
    /// identity.
    ///
    /// Immediate acceptance allocates that identity here. Queued UI acceptance
    /// reuses the identity retained by its prompt, so each path emits the same
    /// content-free trace exactly once.
    pub(super) fn observe_activation_queued_with_id(
        &mut self,
        cid: &AgentId,
        observation_id: tau_proto::ObservationId,
        kind: tau_proto::ActivationKind,
        source_observation: Option<tau_proto::ObservationId>,
        source_call: Option<tau_proto::ToolCallRef>,
    ) {
        let started = Instant::now();
        let succeeded = self.append_activation_queued(
            cid,
            observation_id,
            kind,
            source_observation,
            source_call,
        );
        tracing::trace!(
            target: "tau_harness::prompt_acceptance",
            stage = "activation_append",
            agent_id = %cid,
            event_class = "agent.activation_queued",
            result_class = if succeeded { "success" } else { "failure" },
            activation_append_us = started.elapsed().as_micros(),
            "content-free prompt acceptance precursor"
        );
    }

    /// Allocate and append one queued-activation observation for a prompt that
    /// can drive inference, preserving any identity assigned at an earlier
    /// acceptance point.
    pub(crate) fn ensure_prompt_activation_observed(
        &mut self,
        cid: &AgentId,
        prompt: &mut PendingPrompt,
    ) {
        if prompt.creates_inference_activation() && prompt.activation_observation.is_none() {
            let observation_id = tau_proto::ObservationId::random();
            self.append_prompt_activation_queued(
                cid,
                observation_id,
                prompt.activation_kind(),
                prompt,
            );
            prompt.activation_observation = Some(observation_id);
        }
    }

    /// Append one already-allocated prompt activation, tracing only direct
    /// authenticated UI prompt acceptance.
    pub(super) fn append_prompt_activation_queued(
        &mut self,
        cid: &AgentId,
        observation_id: tau_proto::ObservationId,
        kind: tau_proto::ActivationKind,
        prompt: &PendingPrompt,
    ) {
        if matches!(
            prompt.submission_source,
            tau_proto::PromptSubmissionSource::HumanUi
        ) && prompt.initial_prompt_correlation.is_none()
        {
            self.observe_activation_queued_with_id(cid, observation_id, kind, None, None);
        } else {
            self.append_activation_queued(cid, observation_id, kind, None, None);
        }
    }

    /// Append one activation observation whose identity is already retained by
    /// the queued runtime item.
    pub(super) fn append_activation_queued(
        &mut self,
        cid: &AgentId,
        observation_id: tau_proto::ObservationId,
        kind: tau_proto::ActivationKind,
        source_observation: Option<tau_proto::ObservationId>,
        source_call: Option<tau_proto::ToolCallRef>,
    ) -> bool {
        self.append_best_effort_observation(
            cid,
            observation_id,
            Event::AgentActivationQueued(tau_proto::AgentActivationQueued {
                kind,
                source_observation,
                source_call,
            }),
        )
    }

    /// Preallocate a canonical terminal identity and submit its classification
    /// without making either journal append control terminal publication.
    pub(super) fn observe_tool_terminal(
        &mut self,
        cid: &AgentId,
        call_id: &ToolCallId,
        cause: tau_proto::ToolTerminalCause,
    ) -> Option<tau_proto::ObservationId> {
        if let Some(terminal) = self
            .tool_routing
            .tool_runtime
            .pending_terminal_observations
            .get(call_id)
            .filter(|terminal| terminal.cause == cause)
        {
            return Some(terminal.observation_id);
        }
        let call = self.wait_tool_call_ref(call_id)?;
        let terminal = tau_proto::ObservationId::random();
        self.tool_routing
            .tool_runtime
            .pending_terminal_observations
            .insert(
                call_id.clone(),
                PendingTerminalObservation {
                    observation_id: terminal,
                    cause: cause.clone(),
                },
            );
        self.append_best_effort_observation(
            cid,
            tau_proto::ObservationId::random(),
            Event::AgentToolTerminalClassified(tau_proto::AgentToolTerminalClassified {
                call,
                terminal,
                cause,
            }),
        );
        Some(terminal)
    }

    /// Resolve a declared call to its exact persisted declaration occurrence.
    pub(super) fn persisted_tool_call_ref(
        &self,
        cid: &AgentId,
        call_id: &ToolCallId,
    ) -> Option<tau_proto::ToolCallRef> {
        let agent_id = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)?
            .identity
            .agent_id
            .as_deref()?;
        let events = self
            .session_runtime
            .agent_store
            .agent_events(agent_id)
            .ok()?;
        events.iter().find_map(|record| {
            let Event::ProviderResponseFinished(response) = &record.event else {
                return None;
            };
            response
                .output_items
                .iter()
                .position(
                    |item| matches!(item, ContextItem::ToolCall(call) if &call.call_id == call_id),
                )
                .and_then(|item_index| u32::try_from(item_index).ok())
                .map(|item_index| tau_proto::ToolCallRef {
                    declaration: record.observation_id,
                    item_index,
                })
        })
    }

    /// Publish one terminal result for post-commit runtime settlement.
    pub(super) fn publish_terminal_tool_result(
        &mut self,
        cid: Option<&AgentId>,
        source: Option<&tau_proto::ConnectionId>,
        result: ToolResult,
    ) {
        if result.kind == ToolResultKind::Final
            && let Some(cid) = cid
            && self.tool_terminal_has_open_durable_owner(cid, &result.call_id)
        {
            self.observe_tool_terminal(
                cid,
                &result.call_id,
                tau_proto::ToolTerminalCause::Completed,
            );
        }
        match cid {
            Some(cid) if self.tool_terminal_has_open_durable_owner(cid, &result.call_id) => {
                self.publish_for_agent_from(cid, source, Event::ProviderToolResult(result));
            }
            Some(cid) => {
                self.tool_routing
                    .tool_runtime
                    .tool_agents
                    .entry(result.call_id.clone())
                    .or_insert_with(|| cid.clone());
                let source = self.resolved_publish_source(source);
                self.enqueue_publish(
                    source.as_ref(),
                    Event::ProviderToolResult(result.clone()),
                    false,
                    false,
                    None,
                );
            }
            None => {
                let source = self.resolved_publish_source(source);
                self.enqueue_publish(
                    source.as_ref(),
                    Event::ProviderToolResult(result.clone()),
                    false,
                    false,
                    None,
                );
            }
        }
    }

    /// Publish one terminal error for post-commit runtime settlement.
    pub(super) fn publish_terminal_tool_error(
        &mut self,
        cid: Option<&AgentId>,
        source: Option<&tau_proto::ConnectionId>,
        error: ToolError,
    ) {
        self.publish_terminal_tool_error_with_cause(
            cid,
            source,
            error,
            tau_proto::ToolTerminalCause::ToolError,
        )
    }

    /// Publish one terminal error with an explicit runtime classification.
    pub(super) fn publish_terminal_tool_error_with_cause(
        &mut self,
        cid: Option<&AgentId>,
        source: Option<&tau_proto::ConnectionId>,
        error: ToolError,
        cause: tau_proto::ToolTerminalCause,
    ) {
        if let Some(cid) = cid
            && self.tool_terminal_has_open_durable_owner(cid, &error.call_id)
        {
            self.observe_tool_terminal(cid, &error.call_id, cause);
        }
        match cid {
            Some(cid) if self.tool_terminal_has_open_durable_owner(cid, &error.call_id) => {
                self.publish_for_agent_from(cid, source, Event::ProviderToolError(error));
            }
            Some(cid) => {
                self.tool_routing
                    .tool_runtime
                    .tool_agents
                    .entry(error.call_id.clone())
                    .or_insert_with(|| cid.clone());
                let source = self.resolved_publish_source(source);
                self.enqueue_publish(
                    source.as_ref(),
                    Event::ProviderToolError(error),
                    false,
                    false,
                    None,
                );
            }
            None => {
                let source = self.resolved_publish_source(source);
                self.enqueue_publish(
                    source.as_ref(),
                    Event::ProviderToolError(error),
                    false,
                    false,
                    None,
                );
            }
        }
    }

    /// Return whether `call_id` has an unresolved durable tool-call node owned
    /// by `cid`.
    pub(crate) fn tool_terminal_has_open_durable_owner(
        &self,
        cid: &AgentId,
        call_id: &ToolCallId,
    ) -> bool {
        self.agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| agent.identity.agent_id.as_deref())
            .and_then(|agent_id| self.session_runtime.agent_store.agent(agent_id))
            .is_some_and(|tree| {
                tree.unresolved_foreground_tool_calls()
                    .iter()
                    .any(|call| &call.call_id == call_id)
            })
    }

    pub(super) fn publish_terminal_background_error(
        &mut self,
        cid: &AgentId,
        source: Option<&tau_proto::ConnectionId>,
        error: ToolBackgroundError,
    ) {
        self.publish_for_agent_from(cid, source, Event::ToolBackgroundError(error.clone()));
        self.record_wait_background_error(error, None);
    }

    /// Like [`publish_for_agent`] but lets the caller record source metadata on
    /// the persisted record. Peer reports retain their authenticated extension
    /// source; derived canonical terminal facts use the harness source. The
    /// snap-to-`cid`-head step keeps cross-conversation tool activity from
    /// folding onto the wrong tree branch — without it, a sibling side conv
    /// that just navigated `tree.head` would steal the parent of the next
    /// tree-folding event.
    pub(super) fn publish_for_agent_from(
        &mut self,
        cid: &AgentId,
        source: Option<&tau_proto::ConnectionId>,
        event: Event,
    ) {
        // Stamp the publish with `cid`. The fold reads the
        // agent's `head` as the explicit parent node in
        // `commit_event`, so cross-conversation publishes no longer
        // need a `UiNavigateTree` round-trip to bounce the global
        // write cursor. After the commit, the post-commit hook
        // also syncs `c.head` automatically — the trailing
        // read-tree-and-update idiom is gone entirely.
        //
        // Re-stamp tool events with the owning agent's
        // originator so subscribers can tell main-agent tool
        // activity from sub-agent tool activity without having to
        // map `call_id` back to a conversation themselves. Construction
        // sites can leave `originator` as the default — this is the
        // single point of truth.
        let event = if let Some(originator) = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .map(|c| c.identity.originator.clone())
        {
            stamp_tool_event_originator(event, originator)
        } else {
            event
        };
        self.publish_event_for_agent(cid, source, event);
    }

    /// Publishes an event to both the event bus and the event log.
    /// Convenience wrapper that uses the event's default persistence metadata
    /// and never marks the publish as `must_pass`.
    pub(crate) fn publish_event(&mut self, source: Option<&tau_proto::ConnectionId>, event: Event) {
        let source = self.resolved_publish_source(source);
        let persist = event.defaults_to_persist();
        self.enqueue_publish(source.as_ref(), event, persist, false, None);
    }

    pub(super) fn resolved_publish_source(
        &self,
        source: Option<&tau_proto::ConnectionId>,
    ) -> Option<ConnectionId> {
        source
            .cloned()
            .or_else(|| self.runtime_io.publication.derived_source.clone())
    }

    pub(super) fn mint_agent_runtime_incarnation(&mut self) -> u64 {
        let incarnation = self.agent_runtime.agent_registry.next_runtime_incarnation;
        self.agent_runtime.agent_registry.next_runtime_incarnation = self
            .agent_runtime
            .agent_registry
            .next_runtime_incarnation
            .checked_add(1)
            .expect("agent runtime incarnation space exhausted");
        incarnation
    }

    pub(super) fn with_derived_publish_source<T>(
        &mut self,
        source: Option<ConnectionId>,
        body: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let previous_source = self.runtime_io.publication.derived_source.clone();
        if source.is_some() {
            self.runtime_io.publication.derived_source = source;
        }
        let output = body(self);
        self.runtime_io.publication.derived_source = previous_source;
        output
    }

    /// Like [`Harness::publish_event`] but tags the publish with the
    /// originating agent. After the event commits, the
    /// harness syncs that agent's cached `head` to the
    /// freshly-folded `tree.head()` — so callers don't need to read
    /// the tree themselves (which would race the interception chain
    /// when a publish parks).
    pub(super) fn publish_event_for_agent(
        &mut self,
        cid: &AgentId,
        source: Option<&tau_proto::ConnectionId>,
        event: Event,
    ) {
        self.publish_event_for_agent_with_completion(cid, source, event, None, false);
    }

    pub(super) fn publish_event_for_agent_with_completion(
        &mut self,
        cid: &AgentId,
        source: Option<&tau_proto::ConnectionId>,
        event: Event,
        completion: Option<AgentPublishCompletion>,
        notify_watchers: bool,
    ) {
        if let Event::ProviderResponseFinished(finished) = &event
            && (matches!(
                finished.stop_reason,
                tau_proto::ProviderStopReason::Error
                    | tau_proto::ProviderStopReason::RepetitionDetected
            ) || finished.failure_kind.is_some()
                || finished.error.is_some())
            && let Some(correlation) = self
                .prompt_coordination
                .prompt_runtime
                .pending_initial_correlations
                .remove(cid)
        {
            self.publish_initial_prompt_failed(
                correlation,
                tau_proto::AgentPromptFailureStage::Submission,
                "failed to materialize initial prompt",
            );
        }
        if !self.agent_runtime.agent_registry.agents.contains_key(cid) {
            // The conversation was torn down between when the
            // caller looked it up and now (e.g. side conv that
            // raced its own teardown with a late tool result).
            // Fall back to a plain publish so the event still
            // reaches the bus / log; we just can't stamp a parent
            // for it.
            tracing::warn!(
                target: "tau_harness",
                event = %event.name(),
                cid = %cid,
                "publish_event_for_agent called with unknown cid; \
                 publishing without parent stamp",
            );
            self.publish_event(source, event);
            return;
        }
        let mut persist = event.defaults_to_persist();
        // Accounting lifecycle facts are harness-authored authority. Do not
        // inherit the peer/provider source of the publication whose
        // post-commit continuation generated them.
        let source = if matches!(
            event,
            Event::AgentOuterTurnStarted(_) | Event::AgentOuterTurnFinished(_)
        ) {
            None
        } else {
            self.resolved_publish_source(source)
        };
        let agent_id = self.agent_id_for_event(&event).or_else(|| {
            self.agent_runtime
                .agent_registry
                .agents
                .get(cid)
                .and_then(|conv| conv.identity.agent_id.as_ref())
                .cloned()
                .map(crate::parse_agent_id)
        });
        let must_pass = matches!(
            completion,
            Some(
                AgentPublishCompletion::GatedFinal { .. }
                    | AgentPublishCompletion::InitialPromptSubmission { .. }
            )
        );
        let suppress_activation_dispatch = completion.as_ref().is_some_and(|completion| {
            !matches!(
                completion,
                AgentPublishCompletion::InitialPromptSubmission { .. }
            )
        });
        let prompt_id = match &event {
            Event::ProviderResponseFinished(response) => Some(&response.agent_prompt_id),
            Event::AgentPromptTerminated(terminated) => Some(&terminated.agent_prompt_id),
            _ => None,
        };
        let fold_parent = completion
            .as_ref()
            .and_then(|completion| match completion {
                AgentPublishCompletion::OutputLengthSteer { batch_parent, .. } => {
                    Some(tau_core::AgentEventParent::from_head(*batch_parent))
                }
                AgentPublishCompletion::ReactiveContextRecoveryStart { checkpoint, .. } => {
                    Some(tau_core::AgentEventParent::from_head(checkpoint.through))
                }
                _ => None,
            })
            .or_else(|| {
                prompt_id
                    .and_then(|prompt_id| {
                        self.agent_runtime
                            .agent_registry
                            .agents
                            .get(cid)
                            .and_then(|agent| agent.identity.agent_id.as_deref())
                            .and_then(|agent_id| self.session_runtime.agent_store.agent(agent_id))
                            .and_then(|tree| tree.marked_inference_through(prompt_id))
                    })
                    .map(tau_core::AgentEventParent::from_head)
            });
        persist |= matches!(event, Event::AgentPromptTerminated(_)) && fold_parent.is_some();
        let sync = Some(ConversationHeadSync {
            cid: cid.clone(),
            agent_id,
            session_generation: self.session_runtime.current_session_generation,
            fold_parent,
            suppress_activation_dispatch,
            continuation: completion
                .map(Box::new)
                .map(PostCommitContinuation::AgentPublish),
            notify_watchers,
        });
        self.enqueue_publish(source.as_ref(), event, persist, must_pass, sync);
    }

    /// Updates runtime prompt-route bookkeeping from the compact durable owner.
    pub(super) fn note_agent_prompt_started(&mut self, prompt: &tau_proto::AgentPromptStarted) {
        if let Some(cid) = self
            .prompt_coordination
            .prompt_runtime
            .agents
            .get(&prompt.agent_prompt_id)
            .cloned()
        {
            if self
                .prompt_coordination
                .prompt_runtime
                .pending_initial_correlations
                .get(&cid)
                .is_some_and(|correlation| {
                    prompt.ctx_id.as_deref() == Some(correlation.ctx_id.as_str())
                })
            {
                self.prompt_coordination
                    .prompt_runtime
                    .pending_initial_correlations
                    .remove(&cid);
            }
            if let Some(conv) = self.agent_runtime.agent_registry.agents.get_mut(&cid) {
                conv.dispatch.last_prompt_id = Some(prompt.agent_prompt_id.clone());
            }
        }
    }

    pub(super) fn track_provider_prompt_request(
        &mut self,
        event: &Event,
        provider_connection_id: tau_proto::ConnectionId,
    ) {
        let Some((agent_prompt_id, model)) = (match event {
            Event::AgentPromptCreated(prompt) => Some((&prompt.agent_prompt_id, &prompt.model)),
            _ => None,
        }) else {
            return;
        };
        let rates = self
            .provider_runtime
            .models_by_extension
            .get(&provider_connection_id)
            .and_then(|models| {
                models
                    .iter()
                    .rfind(|candidate| candidate.id == *model)
                    .map(ProviderModelInfo::estimated_api_cost_rates)
            })
            .unwrap_or_else(|| {
                tracing::warn!(
                    target: "tau_harness",
                    %provider_connection_id,
                    %model,
                    %agent_prompt_id,
                    "successful provider route has no matching pricing snapshot; \
                     using estimated API cost fallback"
                );
                tau_proto::ESTIMATED_API_COST_FALLBACK
            });
        self.provider_runtime
            .pending_prompts
            .insert(agent_prompt_id.clone(), provider_connection_id);
        self.prompt_coordination
            .prompt_runtime
            .estimated_cost_rates
            .insert(agent_prompt_id.clone(), rates);
    }

    /// Idempotently disposes runtime state allocated while materializing a
    /// prompt that will never reach a provider.
    pub(super) fn dispose_prompt_dispatch_bookkeeping(
        &mut self,
        agent_prompt_id: &AgentPromptId,
    ) -> Option<AgentId> {
        self.prompt_coordination
            .prompt_runtime
            .pending_dispatches
            .remove(agent_prompt_id);
        self.provider_runtime
            .cache_residency
            .drop_prompt(agent_prompt_id);
        let cid = self
            .prompt_coordination
            .prompt_runtime
            .agents
            .remove(agent_prompt_id.as_str());
        self.provider_runtime
            .pending_prompts
            .remove(agent_prompt_id);
        self.prompt_coordination
            .prompt_runtime
            .operations
            .remove(agent_prompt_id);
        self.prompt_coordination
            .prompt_runtime
            .context_limits
            .remove(agent_prompt_id);
        self.prompt_coordination
            .prompt_runtime
            .context_size_alerts
            .remove(agent_prompt_id);
        self.prompt_coordination
            .prompt_runtime
            .compaction_policies
            .remove(agent_prompt_id);
        self.prompt_coordination
            .prompt_runtime
            .compaction_projected_tokens
            .remove(agent_prompt_id);
        self.prompt_coordination
            .prompt_runtime
            .estimated_cost_rates
            .remove(agent_prompt_id);
        self.clear_prompt_tool_snapshot(agent_prompt_id);
        if let Some(model) = self
            .prompt_coordination
            .prompt_runtime
            .models
            .remove(agent_prompt_id)
        {
            self.session_runtime
                .current_session_state
                .token_usage
                .total
                .requests = self
                .session_runtime
                .current_session_state
                .token_usage
                .total
                .requests
                .saturating_sub(1);
            if let Some(counts) = self
                .session_runtime
                .current_session_state
                .token_usage
                .by_model
                .get_mut(&model)
            {
                counts.requests = counts.requests.saturating_sub(1);
            }
        }
        if let Some(cid) = cid.as_ref()
            && let Some(conv) = self.agent_runtime.agent_registry.agents.get_mut(cid)
        {
            if conv.dispatch.in_flight_prompt.as_ref() == Some(agent_prompt_id) {
                conv.dispatch.in_flight_prompt = None;
                conv.turn.turn_state = AgentTurnState::Idle;
            }
            if conv.dispatch.last_prompt_id.as_ref() == Some(agent_prompt_id) {
                conv.dispatch.last_prompt_id = None;
            }
        }
        cid
    }

    pub(super) fn recover_failed_provider_prompt_route(
        &mut self,
        event: &Event,
        provider_connection_id: &tau_proto::ConnectionId,
        reason: &str,
    ) {
        let Event::AgentPromptCreated(prompt) = event else {
            return;
        };
        let started = tau_proto::AgentPromptStarted::from(prompt);
        self.recover_failed_provider_prompt_route_metadata(
            &started,
            provider_connection_id,
            reason,
        );
    }

    fn recover_failed_provider_prompt_route_metadata(
        &mut self,
        prompt: &tau_proto::AgentPromptStarted,
        provider_connection_id: &tau_proto::ConnectionId,
        reason: &str,
    ) {
        let agent_prompt_id = prompt.agent_prompt_id.clone();
        let cid = self
            .prompt_coordination
            .prompt_runtime
            .agents
            .get(&agent_prompt_id)
            .cloned();
        let failed_compaction = cid.as_ref().and_then(|cid| {
            self.agent_runtime
                .agent_registry
                .agents
                .get(cid)
                .and_then(|agent| match &agent.dispatch.activation_dispatch {
                    path_crate_agent::ActivationDispatchState::Running {
                        id,
                        cut,
                        resume_through,
                        compact_prompt_id: prompt_id,
                        ..
                    } if prompt_id == &agent_prompt_id => {
                        Some((cid.clone(), id.clone(), *cut, *resume_through))
                    }
                    _ => None,
                })
        });
        self.remember_ephemeral_provider_prompt(&agent_prompt_id);
        self.dispose_prompt_dispatch_bookkeeping(&agent_prompt_id);
        self.emit_harness_failure(&format!(
            "provider prompt route failed for `{agent_prompt_id}` via `{provider_connection_id}`: {reason}"
        ));
        if let Some((cid, transaction_id, cut, resume_through)) = failed_compaction {
            self.publish_for_agent(
                &cid,
                Event::AgentStandaloneCompactionFailed(
                    tau_proto::AgentStandaloneCompactionFailed {
                        agent_id: prompt.agent_id.clone(),
                        transaction_id,
                        cut,
                        reason: tau_proto::StandaloneCompactionFailureReason::RouteFailed,
                        resume_through,
                    },
                ),
            );
            return;
        };
        if let Some(cid) = cid {
            if self
                .agent_runtime
                .agent_registry
                .agents
                .get(&cid)
                .is_some_and(|agent| {
                    matches!(
                        agent.dispatch.activation_dispatch,
                        crate::agent::ActivationDispatchState::DispatchUncertain { .. }
                    )
                })
            {
                self.terminalize_unroutable_owned_dispatch(&cid, Some(&prompt.model));
            } else {
                self.set_agent_turn_state(&cid, AgentTurnState::Idle);
                self.try_advance_queue();
            }
        } else {
            self.try_advance_queue();
        }
    }

    pub(super) fn prompt_dispatch_runtime_matches(
        &self,
        sync: &ConversationHeadSync,
        continuation: &PromptDispatchAuthority,
        require_compact_fact: bool,
    ) -> bool {
        if sync.session_generation != self.session_runtime.current_session_generation
            || continuation.started.session_id != self.session_runtime.current_session_id
            || !self
                .prompt_coordination
                .prompt_runtime
                .pending_dispatches
                .contains(&continuation.started.agent_prompt_id)
            || self
                .provider_runtime
                .model_routes
                .get(&continuation.started.model)
                != Some(&continuation.provider_connection_id)
        {
            return false;
        }
        let Some(agent) = self.agent_runtime.agent_registry.agents.get(&sync.cid) else {
            return false;
        };
        if agent.dispatch.terminating
            || agent.dispatch.pending_cancel.is_some()
            || agent.identity.runtime_incarnation != continuation.runtime_incarnation
            || agent.identity.session_id != self.session_runtime.current_session_id
            || agent.identity.agent_id.as_deref() != Some(continuation.started.agent_id.as_str())
            || sync.agent_id.as_ref() != Some(&continuation.started.agent_id)
        {
            return false;
        }
        let owner_matches = match (
            continuation.started.operation,
            &agent.dispatch.activation_dispatch,
        ) {
            (
                tau_proto::PromptOperation::Inference,
                path_crate_agent::ActivationDispatchState::DispatchUncertain {
                    agent_prompt_id,
                    model,
                    operation,
                    ..
                },
            ) => {
                agent_prompt_id == &continuation.started.agent_prompt_id
                    && model.as_ref() == Some(&continuation.started.model)
                    && *operation == Some(continuation.started.operation)
            }
            (
                tau_proto::PromptOperation::StandaloneCompaction,
                path_crate_agent::ActivationDispatchState::Running {
                    compact_prompt_id,
                    model,
                    ..
                },
            ) => {
                compact_prompt_id == &continuation.started.agent_prompt_id
                    && model == &continuation.started.model
            }
            _ => false,
        };
        if !owner_matches {
            return false;
        }
        !require_compact_fact
            || self
                .session_runtime
                .agent_store
                .agent(continuation.started.agent_id.as_str())
                .is_some_and(|tree| tree.prompt_started_is_dispatchable(&continuation.started))
    }

    pub(super) fn prompt_publication_is_authorized(
        &self,
        event: &Event,
        sync: Option<&ConversationHeadSync>,
    ) -> bool {
        let prompt_event = matches!(
            event,
            Event::AgentPromptStarted(_) | Event::AgentPromptCreated(_)
        );
        if !prompt_event {
            return true;
        }
        let Some(sync) = sync else {
            return false;
        };
        if let Event::AgentPromptStarted(started) = event
            && let Some(AgentPublishCompletion::OutputLengthPreDeliveryFailure { response, .. }) =
                sync.completion()
        {
            return response.agent_prompt_id == started.agent_prompt_id
                && response.agent_id == started.agent_id
                && self
                    .agent_runtime
                    .agent_registry
                    .agents
                    .get(&sync.cid)
                    .is_some_and(|agent| {
                        matches!(
                            &agent.turn.output_length_continuation,
                            path_crate_agent::OutputLengthContinuationState::Active(continuation)
                                if continuation.plan.agent_prompt_id == started.agent_prompt_id
                                    && continuation.plan.dispatch.model == started.model
                                    && continuation.plan.dispatch.operation == started.operation
                                    && Some(&continuation.plan.owner.outer_turn_id)
                                        == started.outer_turn_id.as_ref()
                        )
                    });
        }
        let Some(continuation) = sync.prompt_dispatch() else {
            return false;
        };
        let Some(phase) = sync.prompt_dispatch_phase() else {
            return false;
        };
        let phase_matches = match (event, phase) {
            (Event::AgentPromptStarted(started), PromptDispatchPhase::Materialization) => {
                started == &continuation.started
            }
            (Event::AgentPromptCreated(prompt), PromptDispatchPhase::Delivery) => {
                prompt.agent_prompt_id == continuation.started.agent_prompt_id
                    && prompt.agent_id == continuation.started.agent_id
                    && prompt.session_id == continuation.started.session_id
                    && prompt.model == continuation.started.model
                    && Some(prompt.model_params) == continuation.started.model_params
                    && prompt.operation == continuation.started.operation
                    && prompt.originator == continuation.started.originator
                    && prompt.ctx_id == continuation.started.ctx_id
            }
            _ => false,
        };
        phase_matches
            && self.prompt_dispatch_runtime_matches(
                sync,
                continuation,
                phase == PromptDispatchPhase::Delivery,
            )
    }

    pub(super) fn fail_prompt_dispatch_continuation(
        &mut self,
        sync: Option<&ConversationHeadSync>,
        reason: &str,
    ) {
        let Some(continuation) = sync.and_then(ConversationHeadSync::prompt_dispatch) else {
            self.emit_harness_failure(reason);
            return;
        };
        let prompt_id = continuation.started.agent_prompt_id.clone();
        let provider_connection_id = continuation.provider_connection_id.clone();
        self.prompt_coordination
            .prompt_runtime
            .pending_dispatches
            .remove(&prompt_id);
        self.recover_failed_provider_prompt_route_metadata(
            &continuation.started,
            &provider_connection_id,
            reason,
        );
    }

    /// Persist one stamped message fact before exposing it to any consumer.
    ///
    /// The ordinary publication path has already resolved interception before
    /// calling this canonical-fact commit path.
    pub(crate) fn commit_message_fact(
        &mut self,
        source: Option<&tau_proto::ConnectionId>,
        event: Event,
    ) -> bool {
        debug_assert_eq!(event.name().category(), &tau_proto::EventCategory::Message);
        let recorded_at = tau_proto::UnixMicros::now();
        let source_id = source.cloned();
        let persisted_agent = match self.persist_message_fact_record(source, &event, recorded_at) {
            Ok(outcome) => outcome,
            Err(error) => {
                tracing::warn!(
                    target: "tau_harness",
                    event = %event.name(),
                    %error,
                    "message fact append failed before delivery"
                );
                self.emit_harness_failure(&format!(
                    "message fact {} failed to persist: {error}",
                    event.name()
                ));
                return false;
            }
        };
        let skip_debug_log = self.event_targets_ephemeral_agent(&event, None);
        if !skip_debug_log && let Some(log) = &mut self.runtime_io.debug_log {
            let result = log.log_published_event(source_id.as_ref(), &event, recorded_at);
            self.observe_debug_log_result(result);
        }

        let seq = self.runtime_io.event_log.reserve_seq();
        #[cfg(test)]
        self.runtime_io.event_log.record_for_test(
            seq,
            recorded_at,
            source_id.clone(),
            event.clone(),
        );
        #[cfg(not(test))]
        let _ = seq;
        let frame = HarnessOutputMessage::deliver_live(recorded_at, event.clone());
        self.runtime_io
            .bus
            .publish_from_excluding_kinds_without_report(source, frame, &[]);
        if let Some((agent_id, outcome)) = persisted_agent {
            self.activate_projected_message_fact(&agent_id, outcome, &event);
        }
        self.with_derived_publish_source(source.cloned(), |harness| {
            harness.react_to_committed_event(source, &event, true, None);
        });
        true
    }

    /// Append a direct agent semantic fact after any explicit lifecycle stop.
    pub(super) fn append_direct_agent_semantic_event(
        &mut self,
        agent_id: &str,
        parent: tau_core::AgentEventParent,
        event: Event,
    ) -> Result<tau_core::AgentAppendOutcome, HarnessError> {
        let creation = match &event {
            Event::AgentStarted(started) => Some(started.clone()),
            _ => None,
        };
        if creation.is_some()
            && self
                .session_runtime
                .agent_store
                .agent_persistence(agent_id)
                .is_durable()
            && !self
                .session_runtime
                .agent_store
                .agent_id_is_reserved(agent_id)
        {
            self.session_runtime
                .agent_store
                .reserve_new_agent(agent_id)
                .map_err(HarnessError::AgentStore)?;
        }
        let outcome = self
            .session_runtime
            .agent_store
            .append_agent_event_at(agent_id, None, parent, event, tau_proto::UnixMicros::now())
            .map_err(HarnessError::AgentStore)?;
        if let Some(started) = creation {
            self.record_agent_creator_topology(&started);
        }
        Ok(outcome)
    }

    /// Folds one committed creation fact into the runtime-only creator graph.
    pub(super) fn record_agent_creator_topology(&mut self, started: &tau_proto::AgentStarted) {
        let outcome = self.agent_runtime.agent_registry.creator_topology.record(
            started.agent_id.clone(),
            started.creator.as_ref(),
            &self.session_runtime.current_session_id,
        );
        match outcome {
            RecordCreatorOutcome::Recorded => {
                self.agent_runtime
                    .agent_registry
                    .cost_ledger
                    .attach_existing_subtree(
                        &started.agent_id,
                        &self.agent_runtime.agent_registry.creator_topology,
                    );
            }
            RecordCreatorOutcome::AlreadyRecorded
            | RecordCreatorOutcome::NoCreatorEdge
            | RecordCreatorOutcome::ForeignSession => {}
            RecordCreatorOutcome::RejectedSelf => {
                tracing::warn!(
                    target: "tau_harness",
                    agent_id = %started.agent_id,
                    "ignoring self-referential authenticated agent creator"
                );
            }
            RecordCreatorOutcome::RejectedCycle => {
                tracing::warn!(
                    target: "tau_harness",
                    agent_id = %started.agent_id,
                    "ignoring cyclic authenticated agent creator"
                );
            }
            RecordCreatorOutcome::Conflict { existing_creator } => {
                tracing::warn!(
                    target: "tau_harness",
                    agent_id = %started.agent_id,
                    %existing_creator,
                    "ignoring conflicting authenticated agent creator"
                );
            }
        }
    }

    /// Seeds the current runtime topology from one validated loaded creation
    /// fact.
    pub(super) fn seed_agent_creator_topology(&mut self, agent_id: &AgentId) {
        let creation = self
            .session_runtime
            .agent_store
            .agent_events(agent_id.as_str())
            .ok()
            .and_then(|events| match events.first().map(|entry| &entry.event) {
                Some(Event::AgentStarted(started)) if started.agent_id == *agent_id => {
                    Some(started.clone())
                }
                _ => None,
            });
        if let Some(creation) = creation {
            self.record_agent_creator_topology(&creation);
        }
    }

    /// Select and append the canonical journal record for one stamped fact.
    pub(super) fn persist_message_fact_record(
        &mut self,
        source: Option<&tau_proto::ConnectionId>,
        event: &Event,
        recorded_at: tau_proto::UnixMicros,
    ) -> Result<Option<(tau_proto::AgentId, tau_core::AgentAppendOutcome)>, HarnessError> {
        let source = source
            .cloned()
            .map(tau_core::PersistedEventSource::Connection);
        let known_agent = event.message_agent_target().and_then(|target| {
            let agent_id = tau_proto::AgentId::parse(target.as_str()).ok()?;
            let has_live_route = self
                .agent_runtime
                .agent_registry
                .agent_routes
                .get(agent_id.as_str())
                .and_then(|cid| self.agent_runtime.agent_registry.agents.get(cid))
                .is_some_and(|agent| agent.identity.agent_id.as_deref() == Some(agent_id.as_str()));
            (has_live_route
                || self
                    .session_runtime
                    .agent_store
                    .agent_is_known_for_routing(agent_id.as_str()))
            .then_some(agent_id)
        });
        if let Some(agent_id) = known_agent {
            let outcome = self
                .session_runtime
                .agent_store
                .append_agent_message_fact_at(
                    agent_id.as_str(),
                    source,
                    event.clone(),
                    recorded_at,
                )?;
            return Ok(Some((agent_id, outcome)));
        } else {
            self.session_runtime
                .store
                .append_session_event_at_with_persistence(
                    self.session_runtime.current_session_id.as_str(),
                    source,
                    event.clone(),
                    recorded_at,
                    self.session_runtime.storage_mode.session_persistence(),
                )?;
        }
        Ok(None)
    }

    /// Place and activate one valid live incoming fact after canonical append.
    pub(super) fn activate_projected_message_fact(
        &mut self,
        agent_id: &tau_proto::AgentId,
        outcome: tau_core::AgentAppendOutcome,
        event: &Event,
    ) {
        let Some(Ok(projection)) = tau_proto::project_message_fact(event) else {
            return;
        };
        let Some(cid) = self
            .agent_runtime
            .agent_registry
            .agent_routes
            .get(agent_id.as_str())
            .cloned()
        else {
            return;
        };
        if outcome.folded_node_id.is_some()
            && let Some(node_id) = outcome.selected_head_id
            && let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&cid)
        {
            agent.identity.head = Some(node_id);
            agent.execution.result_dedup.note_head_advanced_to(node_id);
        }
        if !projection.activates_model
            || self
                .agent_runtime
                .agent_registry
                .agents
                .get(&cid)
                .is_none_or(|agent| agent.dispatch.terminating)
        {
            return;
        }
        let activation = tau_proto::ObservationId::random();
        if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&cid)
            && !agent.dispatch.pending_message_wakes.iter().any(|wake| {
                matches!(
                    wake.source,
                    crate::agent::PendingMessageWakeSource::MessageFact {
                        durable_event_seq: existing,
                    } if existing == outcome.seq
                )
            })
        {
            agent
                .dispatch
                .pending_message_wakes
                .push_back(crate::agent::PendingMessageWake {
                    source: path_crate_agent::PendingMessageWakeSource::MessageFact {
                        durable_event_seq: outcome.seq,
                    },
                    node_id: outcome.folded_node_id,
                    activation_observation: Some(activation),
                    source_observation: Some(outcome.observation_id),
                });
        }
        self.append_activation_queued(
            &cid,
            activation,
            tau_proto::ActivationKind::ExternalMessage,
            Some(outcome.observation_id),
            None,
        );
        self.activate_waits_for(&cid, activation);
        if self.terminalize_uncertain_marked_owner_for_live_activation(&cid) {
            return;
        }
        self.try_advance_queue();
    }

    /// Close an exact response-uncertain marked ordinary owner after a newly
    /// committed live activating occurrence. The terminal publication remains
    /// interceptable, and all runtime cleanup waits for its successful append.
    pub(super) fn terminalize_uncertain_marked_owner_for_live_activation(
        &mut self,
        cid: &AgentId,
    ) -> bool {
        let Some((durable_agent_id, agent_prompt_id, originator)) = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| {
                if agent.dispatch.in_flight_prompt.is_none()
                    && let ActivationDispatchState::DispatchUncertain {
                        owner: InferenceCheckpointOwner::Inference,
                        agent_prompt_id,
                        ..
                    } = &agent.dispatch.activation_dispatch
                {
                    Some((
                        agent.identity.agent_id.clone()?,
                        agent_prompt_id.clone(),
                        agent.identity.originator.clone(),
                    ))
                } else {
                    None
                }
            })
        else {
            return false;
        };
        if self
            .session_runtime
            .agent_store
            .agent(&durable_agent_id)
            .and_then(|tree| tree.marked_inference_through(&agent_prompt_id))
            .is_none()
        {
            return false;
        }
        self.publish_for_agent(
            cid,
            Event::AgentPromptTerminated(AgentPromptTerminated {
                automatic_compaction_decision: None,
                agent_id: crate::parse_agent_id(&durable_agent_id),
                agent_prompt_id,
                reason: AgentPromptTerminationReason::Stale,
                originator,
            }),
        );
        true
    }

    /// Final commit: persist (when applicable), append to the event
    /// log, and broadcast on the bus. Does not consult interception
    /// state — the caller is responsible for getting here only when the chain
    /// has resolved. After broadcast, it runs captured peer-event consumers
    /// and other post-commit reactions, including deferred agent dispatch
    /// and per-publish conversation `head` synchronization.
    pub(crate) fn commit_event(
        &mut self,
        source: Option<&tau_proto::ConnectionId>,
        peer_context: &interception::PeerPublicationContext,
        event: Event,
        persist: bool,
        mut sync_head_for: Option<ConversationHeadSync>,
    ) {
        let mut event = event;
        self.arbitrate_output_length_terminal_cancellation(&mut event, &mut sync_head_for);
        let watch_retirement = sync_head_for
            .as_ref()
            .and_then(ConversationHeadSync::watch_retirement)
            .cloned();
        if let Some(completion) = watch_retirement.as_ref()
            && !watch_retirement_event_matches(&event, completion)
        {
            self.finish_watch_retirement_delivery(completion, false);
            self.emit_harness_failure(
                "watch lifecycle publication was replaced with an invalid event",
            );
            return;
        }
        if !self.prompt_publication_is_authorized(&event, sync_head_for.as_ref()) {
            self.fail_prompt_dispatch_continuation(
                sync_head_for.as_ref(),
                "prompt publication lost its compact-fact delivery authority",
            );
            return;
        }
        if event.message_agent_target().is_some() {
            // ast-grep-ignore: debug-assert-expression-must-not-mutate
            debug_assert!(persist, "canonical message facts must be durable");
            self.commit_message_fact(source, event);
            return;
        }
        if !self.validate_pending_external_receive_before_commit(&event) {
            return;
        }
        if !self.synchronized_inference_checkpoint_has_live_owner(&event, sync_head_for.as_ref()) {
            self.rollback_rejected_activation_successor(&event);
            self.emit_info("dropping stale synchronized inference checkpoint after teardown");
            return;
        }
        let reactive_recovery_claim = matches!(
            sync_head_for
                .as_ref()
                .and_then(ConversationHeadSync::completion),
            Some(AgentPublishCompletion::ReactiveContextRecoveryStart { .. })
        ) && matches!(
            event,
            Event::AgentStandaloneCompactionStarted(tau_proto::AgentStandaloneCompactionStarted {
                trigger: tau_proto::StandaloneCompactionTrigger::ReactiveContextOverflow { .. },
                ..
            })
        );
        if !reactive_recovery_claim && !self.activation_successor_matches_selected_head(&event) {
            self.rollback_rejected_activation_successor(&event);
            self.emit_harness_failure(&format!(
                "dropping stale off-branch activation successor {}",
                event.name()
            ));
            return;
        }
        if sync_head_for.as_ref().is_some_and(|sync| {
            sync.completion().is_some()
                && (sync.session_generation != self.session_runtime.current_session_generation
                    || !self
                        .agent_runtime
                        .agent_registry
                        .agents
                        .get(&sync.cid)
                        .is_some_and(|agent| {
                            agent.identity.session_id == self.session_runtime.current_session_id
                                && !agent.dispatch.terminating
                                && sync.agent_id.as_ref().is_none_or(|agent_id| {
                                    agent.identity.agent_id.as_deref() == Some(agent_id.as_str())
                                })
                        }))
        }) {
            self.emit_info("dropping stale completion publication after agent/session teardown");
            return;
        }
        if let Some(sync) = sync_head_for.as_ref()
            && let Some(batch_parent) = sync.completion().and_then(|completion| match completion {
                AgentPublishCompletion::GatedFinal { batch_parent, .. }
                | AgentPublishCompletion::OutputLengthContinuation { batch_parent, .. }
                | AgentPublishCompletion::OutputLengthSteer { batch_parent, .. } => {
                    Some(*batch_parent)
                }
                _ => None,
            })
            && self.selected_head_for_agent(&sync.cid) != Some(batch_parent)
        {
            self.retain_rejected_agent_publish(sync_head_for.as_ref(), &event);
            self.emit_info("retaining branch-owned publication until its exact parent is selected");
            return;
        }
        let mut commit_timing = CommitEventTiming::new(event.name());
        // When this publish was stamped with a conversation, fold
        // the event onto that agent's branch directly. This
        // skips the `UiNavigateTree` head-bouncing dance that
        // `publish_for_agent_from` used to do — the explicit
        // parent in `apply_event_at` does the same job without
        // touching the global cursor.
        let parent_for_fold =
            if let Some(parent) = sync_head_for.as_ref().and_then(|sync| sync.fold_parent) {
                parent
            } else if sync_head_for.as_ref().is_some_and(|s| {
                self.agent_runtime
                    .agent_registry
                    .agents
                    .get(&s.cid)
                    .is_some_and(|c| c.identity.head.is_none())
            }) {
                tau_core::AgentEventParent::Root
            } else {
                sync_head_for
                    .as_ref()
                    .and_then(|s| {
                        self.agent_runtime
                            .agent_registry
                            .agents
                            .get(&s.cid)
                            .and_then(|c| c.identity.head)
                    })
                    .map(tau_core::AgentEventParent::Under)
                    .unwrap_or(tau_core::AgentEventParent::InheritHead)
            };
        // Stamp once and share with every downstream observer: the durable
        // record on disk, the debug JSONL line, and the wire delivery.
        // Sampling the clock separately would let timing analyses
        // disagree with what live subscribers saw.
        let source_id = source.cloned();
        let recorded_at = tau_proto::UnixMicros::now();
        let persistence_source = match &event {
            // A configured peer's durable request must not retain its run-local
            // connection id. The stable configured publisher is the only identity
            // that remains meaningful when the restore fact is replayed.
            Event::ToolRequest(_) => peer_context
                .extension
                .as_ref()
                .map(|extension| {
                    tau_core::PersistedEventSource::Extension(extension.publisher.clone())
                })
                .or_else(|| {
                    source
                        .cloned()
                        .map(tau_core::PersistedEventSource::Connection)
                }),
            _ => source
                .cloned()
                .map(tau_core::PersistedEventSource::Connection),
        };
        let semantic_persist_started = Instant::now();
        let append_result = self.persist_semantic_event(
            persistence_source,
            &event,
            persist,
            parent_for_fold,
            sync_head_for.as_ref(),
            recorded_at,
        );
        commit_timing.semantic_persist = semantic_persist_started.elapsed();
        let append_outcome = match append_result {
            Ok(append_outcome) => append_outcome,
            Err(error) => {
                commit_timing.result = CommitEventTimingResult::SemanticPersistError;
                self.rollback_rejected_activation_successor(&event);
                self.clear_rejected_eager_compaction_start(&event);
                self.rollback_failed_wait_compaction_terminal(&event);
                self.retain_rejected_agent_publish(sync_head_for.as_ref(), &event);
                if !matches!(
                    sync_head_for
                        .as_ref()
                        .and_then(ConversationHeadSync::completion),
                    Some(AgentPublishCompletion::OutputLengthDormantRepair { .. })
                ) {
                    self.retain_rejected_outer_turn_finish(&event);
                }
                if semantic_event_router::session_membership_id_for_event(&event)
                    .is_some_and(|session_id| session_id == self.session_runtime.current_session_id)
                {
                    self.agent_runtime.agent_registry.roster_valid = false;
                }
                tracing::warn!(
                    target: "tau_harness",
                    event = %event.name(),
                    %error,
                    "dropping event rejected by session store"
                );
                self.emit_harness_failure(&format!(
                    "event {} rejected by session store: {error}",
                    event.name()
                ));
                self.fail_pending_external_receive(
                    &event,
                    "peer receive projection failed to persist",
                    tau_proto::ExternalAgentMessageFailure::Rejected,
                );
                if sync_head_for
                    .as_ref()
                    .is_some_and(|sync| sync.prompt_dispatch().is_some())
                {
                    self.fail_prompt_dispatch_continuation(
                        sync_head_for.as_ref(),
                        "compact prompt materialization failed to commit",
                    );
                }
                if let Some(completion) = watch_retirement.as_ref() {
                    self.finish_watch_retirement_delivery(completion, false);
                }
                return;
            }
        };
        let seq = self.runtime_io.event_log.reserve_seq();
        #[cfg(test)]
        self.runtime_io.event_log.record_for_test(
            seq,
            recorded_at,
            source_id.clone(),
            event.clone(),
        );
        #[cfg(not(test))]
        let _ = seq;
        let debug_log_started = Instant::now();
        let skip_debug_log = peer_context
            .extension
            .as_ref()
            .is_some_and(|extension| extension.shell_report_targets_ephemeral)
            || self.event_targets_ephemeral_agent(&event, sync_head_for.as_ref());
        if !skip_debug_log && let Some(log) = &mut self.runtime_io.debug_log {
            let result = log.log_published_event(source_id.as_ref(), &event, recorded_at);
            self.observe_debug_log_result(result);
        }
        commit_timing.debug_log = debug_log_started.elapsed();
        if let Event::SessionAgentLoaded(loaded) = &event
            && loaded.session_id == self.session_runtime.current_session_id
        {
            self.agent_runtime
                .agent_registry
                .roster_loaded
                .insert(loaded.agent_id.clone());
            self.agent_runtime
                .agent_registry
                .roster_ever_loaded
                .insert(loaded.agent_id.clone());
        } else if let Event::SessionAgentUnloaded(unloaded) = &event
            && unloaded.session_id == self.session_runtime.current_session_id
        {
            self.agent_runtime
                .agent_registry
                .roster_loaded
                .remove(&unloaded.agent_id);
        }
        if let Event::AgentPromptStarted(prompt) = &event {
            self.note_agent_prompt_started(prompt);
        }
        if let Some(sync) = sync_head_for.as_ref()
            && let Some(c) = self.agent_runtime.agent_registry.agents.get_mut(&sync.cid)
        {
            match (&event, append_outcome.as_ref()) {
                (Event::AgentHeadMoved(moved), _) => {
                    c.identity.head = moved.head.as_option();
                    c.identity.branch_generation = c.identity.branch_generation.saturating_add(1);
                    c.execution.loop_guard.invalidate_branch();
                    c.dispatch
                        .pending_prompts
                        .retain(|prompt| !prompt.is_loop_guard());
                }
                (_, Some(outcome)) if outcome.folded_node_id.is_some() => {
                    // Only advance the agent's own branch cursor when
                    // the event produced a tree node. `tree.head()` is the
                    // *global* write cursor and may sit on a sibling
                    // agent's last fold; syncing to it after a
                    // non-folding event (e.g. `ProviderResponseFinished` with
                    // only tool calls) would graft this agent's next
                    // tool request onto the wrong branch and produce orphan
                    // ToolUse blocks downstream.
                    c.identity.head = outcome.selected_head_id;
                    // Keep the dedup map's "built for" cursor in lockstep with
                    // the just-folded linear extension. The dedup-decision
                    // path already inserted any new (hash, call_id) entry
                    // before the publish, so the map's contents already match
                    // what a fresh rebuild from this new head would produce.
                    // Bumping the cursor here lets the next tool result skip
                    // the rebuild entirely (the steady-state hot path).
                    //
                    // We pass *every* fold through this hook, including ones
                    // that didn't touch the dedup map (a user message from
                    // session re-init or a message projection).
                    // [`ResultDedupMap::note_head_advanced_to`] guards
                    // against the dangerous case — `built_for == None` plus a
                    // non-dedup-eligible fold — by skipping the bump, so the
                    // rebuild still triggers on the next dedup intake. Don't
                    // gate this call on the event variant: that would re-couple
                    // `commit_event` to per-tool semantics that the dedup
                    // module deliberately owns.
                    if let Some(node_id) = outcome.selected_head_id {
                        c.execution.result_dedup.note_head_advanced_to(node_id);
                    }
                }
                _ => {}
            }
        }
        let commits_inference_activation = matches!(
            event,
            Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                inference_activation: true,
                ..
            }) | Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
                inference_activation: true,
                ..
            }) | Event::AgentPromptSteered(tau_proto::AgentPromptSteered {
                inference_activation: true,
                ..
            })
        );
        if matches!(
            &event,
            Event::AgentPromptSteered(tau_proto::AgentPromptSteered {
                internal_kind: Some(tau_proto::InternalPromptKind::OutputLengthContinuation),
                ..
            })
        ) && let Some(through) = append_outcome
            .as_ref()
            .and_then(|outcome| outcome.folded_node_id)
            .map(tau_proto::AgentHead::Node)
            && let Some(sync) = sync_head_for.as_ref()
            && !matches!(
                sync.completion(),
                Some(AgentPublishCompletion::OutputLengthDormantRepair { .. })
            )
            && let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&sync.cid)
            && matches!(
                agent.turn.output_length_continuation,
                path_crate_agent::OutputLengthContinuationState::Planned(_)
            )
        {
            let path_crate_agent::OutputLengthContinuationState::Planned(plan) =
                std::mem::take(&mut agent.turn.output_length_continuation)
            else {
                unreachable!("matched planned output-length continuation");
            };
            agent.turn.output_length_continuation =
                path_crate_agent::OutputLengthContinuationState::OwnerReady(
                    path_crate_agent::OutputLengthContinuationDispatch { plan, through },
                );
        }
        if commits_inference_activation
            && let Some(sync) = sync_head_for
                .as_ref()
                .filter(|sync| !sync.suppress_activation_dispatch)
        {
            let activation_through = append_outcome
                .as_ref()
                .and_then(|outcome| outcome.folded_node_id)
                .map(tau_proto::AgentHead::Node);
            if let Some(outcome) = append_outcome.as_ref() {
                self.enqueue_committed_activation_occurrence(
                    sync.cid.clone(),
                    outcome.seq,
                    activation_through,
                );
            }
        }
        let agent_publish_completion = sync_head_for
            .as_ref()
            .and_then(|sync| {
                sync.completion()
                    .cloned()
                    .map(|completion| (sync.cid.clone(), completion))
            })
            .map(|(cid, completion)| {
                let through = append_outcome
                    .as_ref()
                    .and_then(|outcome| outcome.folded_node_id)
                    .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
                (cid, completion, through)
            });
        let bus_enqueue_started = Instant::now();
        let prompt_provider_route = matches!(event, Event::AgentPromptCreated(_))
            .then(|| {
                sync_head_for
                    .as_ref()
                    .filter(|sync| {
                        sync.prompt_dispatch_phase() == Some(PromptDispatchPhase::Delivery)
                    })
                    .and_then(ConversationHeadSync::prompt_dispatch)
                    .map(|continuation| continuation.provider_connection_id.clone())
            })
            .flatten();
        if let Some(provider_connection_id) = prompt_provider_route {
            if !self.prompt_publication_is_authorized(&event, sync_head_for.as_ref()) {
                self.fail_prompt_dispatch_continuation(
                    sync_head_for.as_ref(),
                    "prompt delivery authority changed before provider send",
                );
                return;
            }
            if let Event::AgentPromptCreated(prompt) = &event {
                self.preempt_cache_refresh_for_prompt(prompt);
                let model_info = self.provider_runtime.model_info.get(&prompt.model).cloned();
                self.provider_runtime.cache_residency.track_prompt(
                    provider_connection_id.clone(),
                    prompt,
                    model_info.as_ref(),
                );
            }
            // Provider-owned prompt execution is point-to-point: observers still
            // see the transient work envelope, but execution clients do not all race
            // to consume it. The owning provider gets the exact same delivery
            // payload via a directed route so replay/live delivery metadata
            // matches the subscribed-provider path.
            let execution_kinds = [ClientKind::Provider];
            let provider_frame = HarnessOutputMessage::deliver_live(recorded_at, event.clone());
            let provider_frame = self
                .runtime_io
                .bus
                .publish_event_from_excluding_kinds_lazy_without_report(
                    source,
                    provider_frame,
                    &execution_kinds,
                    || {
                        HarnessOutputMessage::deliver_live(
                            recorded_at,
                            event_without_provider_image_bytes(&event),
                        )
                    },
                );
            match self
                .runtime_io
                .bus
                .send_to(&provider_connection_id, source, provider_frame)
            {
                Ok(report) if !report.delivered_to.is_empty() => {
                    self.track_provider_prompt_request(&event, provider_connection_id);
                }
                Ok(report) => {
                    tracing::warn!(
                        target: "tau_harness",
                        event = %event.name(),
                        provider_connection_id = %provider_connection_id,
                        ?report,
                        "provider prompt route did not deliver"
                    );
                    self.recover_failed_provider_prompt_route(
                        &event,
                        &provider_connection_id,
                        "no provider connection accepted the prompt",
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        target: "tau_harness",
                        event = %event.name(),
                        provider_connection_id = %provider_connection_id,
                        %error,
                        "provider prompt route failed"
                    );
                    self.recover_failed_provider_prompt_route(
                        &event,
                        &provider_connection_id,
                        &error.to_string(),
                    );
                }
            }
        } else if matches!(event, Event::AgentPromptCreated(_)) {
            // Provider prompts are never broadcast. A route can disappear while
            // PromptStarted/PromptCreated is parked in interception, after the
            // pre-materialization ownership check. Keep observer delivery, but
            // exclude every provider and fail the exact durable owner before any
            // remote client can see the request.
            let execution_kinds = [ClientKind::Provider];
            let admission_frame = HarnessOutputMessage::deliver_live(recorded_at, event.clone());
            self.runtime_io
                .bus
                .publish_event_from_excluding_kinds_lazy_without_report(
                    source,
                    admission_frame,
                    &execution_kinds,
                    || {
                        HarnessOutputMessage::deliver_live(
                            recorded_at,
                            event_without_provider_image_bytes(&event),
                        )
                    },
                );
            let unavailable_route = tau_proto::ConnectionId::parse("unavailable-model-route")
                .expect("fixed unavailable route must satisfy the connection identifier grammar");
            self.recover_failed_provider_prompt_route(
                &event,
                &unavailable_route,
                "captured provider-qualified model has no route",
            );
        } else if matches!(
            event,
            Event::ProviderToolResult(_) | Event::ToolResult(_) | Event::ToolBackgroundResult(_)
        ) {
            let observer_frame = HarnessOutputMessage::deliver_live(
                recorded_at,
                event_without_provider_image_bytes(&event),
            );
            // Raw provider and generic result data is not a UI payload. UIs
            // receive the separately published payload-free display projection.
            self.runtime_io
                .bus
                .publish_from_excluding_kinds_without_report(
                    source,
                    observer_frame,
                    &[ClientKind::Ui],
                );
        } else {
            let observer_frame = HarnessOutputMessage::deliver_live(
                recorded_at,
                event_without_provider_image_bytes(&event),
            );
            self.runtime_io
                .bus
                .publish_from_excluding_kinds_without_report(source, observer_frame, &[]);
        }
        if let Event::AgentPromptCreated(prompt) = &event {
            self.prompt_coordination
                .prompt_runtime
                .pending_dispatches
                .remove(&prompt.agent_prompt_id);
        }
        commit_timing.bus_enqueue = bus_enqueue_started.elapsed();
        let post_commit_started = Instant::now();
        if let Event::ShellCommandProgress(progress) = &event {
            self.release_pending_ephemeral_shell_canonical_marker(&progress.command_id);
        }
        if let Event::ShellCommandFinished(finished) = &event {
            self.release_pending_ephemeral_shell_canonical_marker(&finished.command_id);
            self.ui_runtime
                .active_ui_shell_command_ids
                .remove(&finished.command_id);
            if self
                .ui_runtime
                .pending_ui_shell_output_injections
                .remove(&finished.command_id)
            {
                self.inject_user_shell_output(finished);
            }
        }
        if let Err(error) = self.dispatch_internal_tool_event(&event) {
            self.emit_harness_failure(&format!("internal tool event handler failed: {error}"));
        }
        self.process_committed_peer_event(source, peer_context, &event);
        self.with_derived_publish_source(source.cloned(), |harness| {
            harness.react_to_committed_event(source, &event, persist, append_outcome.as_ref());
        });
        if sync_head_for
            .as_ref()
            .is_some_and(|sync| sync.notify_watchers)
        {
            match &event {
                Event::AgentPromptSteered(steered) => self.notify_agent_watchers_about_user_prompt(
                    steered.agent_id.as_str(),
                    &steered.text,
                ),
                Event::ProviderResponseFinished(response)
                    if let Some(message) =
                        assistant_text_from_output_items(&response.output_items) =>
                {
                    if let Some(cid) =
                        self.runtime_agent_id_for_target_agent(Some(response.agent_id.as_str()))
                    {
                        self.notify_agent_watchers_about_response(&cid, message);
                    }
                }
                _ => {}
            }
        }
        if let Some((cid, completion, through)) = agent_publish_completion {
            #[cfg(feature = "output-length-test-barrier")]
            {
                use crate::output_length_test_barrier::{OutputLengthCommitCut, reach};
                match &completion {
                    AgentPublishCompletion::OutputLengthContinuation { response, .. }
                        if matches!(
                            response.output_length_disposition,
                            tau_proto::OutputLengthDisposition::ContinuationPlanned { .. }
                        ) =>
                    {
                        assert!(
                            self.session_runtime
                                .persistence_owner
                                .as_ref()
                                .is_some_and(|owner| owner
                                    .wait_for_latest_durability_for_test(Duration::from_secs(5))),
                            "planned-response persistence barrier timed out"
                        );
                        reach(OutputLengthCommitCut::AfterPlannedResponse);
                    }
                    AgentPublishCompletion::OutputLengthSteer { .. } => {
                        assert!(
                            self.session_runtime
                                .persistence_owner
                                .as_ref()
                                .is_some_and(|owner| owner
                                    .wait_for_latest_durability_for_test(Duration::from_secs(5))),
                            "continuation-steer persistence barrier timed out"
                        );
                        reach(OutputLengthCommitCut::AfterContinuationSteer);
                    }
                    _ => {}
                }
            }
            self.complete_agent_publish(&cid, completion, through);
        }
        #[cfg(feature = "output-length-test-barrier")]
        if let Some(cut) = crate::output_length_test_barrier::observe_typed_receipt(&event) {
            assert!(
                self.session_runtime.persistence_owner.as_ref().is_some_and(
                    |owner| owner.wait_for_latest_durability_for_test(Duration::from_secs(5))
                ),
                "typed-receipt persistence barrier timed out"
            );
            crate::output_length_test_barrier::reach(cut);
        }
        if let Some(completion) = watch_retirement.as_ref() {
            self.finish_watch_retirement_delivery(completion, true);
        }
        let prompt_materialization = sync_head_for.as_mut().and_then(|sync| {
            (sync.prompt_dispatch_phase() == Some(PromptDispatchPhase::Materialization)
                && matches!(event, Event::AgentPromptStarted(_)))
            .then(|| sync.continuation.take())
            .flatten()
        });
        if let Some(PostCommitContinuation::PromptMaterialization(continuation)) =
            prompt_materialization
        {
            let (prompt, authority) = continuation.into_delivery();
            let prompt = Event::AgentPromptCreated(prompt);
            let sync = sync_head_for.as_ref().expect("prompt sync exists");
            self.enqueue_publish(
                None,
                prompt,
                false,
                true,
                Some(ConversationHeadSync {
                    cid: sync.cid.clone(),
                    agent_id: sync.agent_id.clone(),
                    session_generation: sync.session_generation,
                    fold_parent: None,
                    suppress_activation_dispatch: true,
                    continuation: Some(PostCommitContinuation::PromptDelivery(authority)),
                    notify_watchers: false,
                }),
            );
        }
        self.complete_pending_external_receive(&event);
        commit_timing.post_commit = post_commit_started.elapsed();
        commit_timing.result = CommitEventTimingResult::Ok;
    }
}
