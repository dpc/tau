//! Admits authenticated configured-extension reports and derives canonical
//! committed facts.
//!
//! Source generation, trust validation, and commit-before-semantics ordering
//! remain authoritative here.

use super::*;

impl Harness {
    #[cfg(test)]
    pub(super) fn handle_extension_event_inner(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        event: Event,
    ) -> Result<(), HarnessError> {
        self.handle_extension_event_inner_with_persist(source_id, event, None)
    }

    pub(super) fn handle_extension_event_inner_with_persist(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        event: Event,
        persist_override: Option<bool>,
    ) -> Result<(), HarnessError> {
        self.handle_extension_event_inner_with_admission(
            source_id,
            event,
            persist_override,
            self.current_extension_frame_admission(),
        )
    }

    pub(super) fn handle_extension_event_inner_with_admission(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        event: Event,
        persist_override: Option<bool>,
        admission: ExtensionFrameAdmission,
    ) -> Result<(), HarnessError> {
        let event_name = event.name();
        if event.is_message_report() {
            let authorized = self.extensions.entries.get(source_id).is_some_and(|entry| {
                entry
                    .peer_capabilities
                    .contains(&tau_proto::PeerCapability::MessageBridge)
            });
            if !authorized {
                tracing::warn!(
                    target: "tau_harness",
                    connection_id = %source_id,
                    event = %event_name,
                    "extension lacks message-bridge report authority"
                );
                return Ok(());
            }
            self.handle_extension_fallback_event_with_admission(
                source_id,
                event,
                persist_override,
                admission,
            );
            return Ok(());
        }
        if event_name.category() == &tau_proto::EventCategory::Message {
            // Canonical message facts are harness-authored. Bridges publish the
            // corresponding `message.*_reported` event instead.
            return Ok(());
        }
        if matches!(event, Event::ToolRegister(_) | Event::ToolUnregister(_)) {
            // Canonical tool state is harness-authored. Tool/Core extensions
            // publish the corresponding mutable declaration instead.
            return Ok(());
        }
        if matches!(event, Event::ToolProgress(_)) {
            // Canonical tool progress is harness-authored. Tool/Core extensions
            // publish `tool.progress_reported` observations instead.
            return Ok(());
        }
        if matches!(
            event,
            Event::ActionSchemaPublished(_) | Event::ActionResult(_) | Event::ActionError(_)
        ) {
            // Peers submit declarations and reports. Canonical Action facts are
            // harness-authored.
            return Ok(());
        }
        if matches!(
            event,
            Event::ActionSchemaDeclared(_)
                | Event::ActionResultReported(_)
                | Event::ActionErrorReported(_)
        ) {
            let authorized = self.extensions.entries.get(source_id).is_some_and(|entry| {
                matches!(
                    entry.kind,
                    ClientKind::Provider | ClientKind::Tool | ClientKind::Action | ClientKind::Core
                ) && entry.state != ExtensionState::Disconnected
                    && entry
                        .peer_capabilities
                        .contains(&tau_proto::PeerCapability::ActionProvider)
            });
            if !authorized {
                tracing::warn!(
                    target: "tau_harness",
                    connection_id = %source_id,
                    event = %event_name,
                    "extension lacks Action provider authority"
                );
                return Ok(());
            }
            if matches!(event, Event::ActionSchemaDeclared(_))
                && self.should_stage_extension_capabilities(source_id)
            {
                *self
                    .extensions
                    .pending_action_schema_declarations
                    .entry(source_id.clone())
                    .or_default() += 1;
            }
            let persist = persist_override.unwrap_or_else(|| event.defaults_to_persist());
            self.enqueue_publish_with_admission(
                Some(source_id),
                event,
                persist,
                false,
                None,
                admission,
            );
            return Ok(());
        }
        if source_id != harness_connection_id()
            && matches!(
                event,
                Event::ToolResult(_)
                    | Event::ToolResultDisplay(_)
                    | Event::ToolError(_)
                    | Event::ToolCancelled(_)
            )
        {
            // Peer terminal outcomes use reports. Harness-owned internal tools
            // retain their direct canonical completion path.
            return Ok(());
        }
        if source_id != harness_connection_id()
            && matches!(
                &event,
                Event::ToolResultReported(result)
                    if result.presentation
                        != tau_proto::ToolResultPresentation::ToolPayload
            )
        {
            tracing::warn!(
                target: "tau_harness",
                connection_id = %source_id,
                "rejecting configured extension tool result with harness-only presentation"
            );
            return Ok(());
        }
        if source_id != harness_connection_id()
            && matches!(
                &event,
                Event::ToolErrorReported(error)
                    if error.presentation
                        != tau_proto::ToolResultPresentation::ToolPayload
            )
        {
            tracing::warn!(
                target: "tau_harness",
                connection_id = %source_id,
                "rejecting configured extension tool error with harness-only presentation"
            );
            return Ok(());
        }
        if source_id != harness_connection_id()
            && matches!(
                &event,
                Event::ToolCancelledReported(cancelled)
                    if cancelled.presentation
                        != tau_proto::ToolResultPresentation::ToolPayload
            )
        {
            tracing::warn!(
                target: "tau_harness",
                connection_id = %source_id,
                "rejecting configured extension tool cancellation with harness-only presentation"
            );
            return Ok(());
        }
        if matches!(
            event,
            Event::ToolRegistrationDeclared(_) | Event::ToolUnregistrationDeclared(_)
        ) {
            // This is declarative authorship/resource admission only. Per
            // `SPEC-peer-event-publication`, prefix/schema/ownership
            // validation and registry mutation run from the committed-event
            // consumer, never from this generic Emit intake path.
            let authorized = self.extensions.entries.get(source_id).is_some_and(|entry| {
                matches!(entry.kind, ClientKind::Tool | ClientKind::Core)
                    && entry.state != ExtensionState::Disconnected
            });
            if !authorized {
                tracing::warn!(
                    target: "tau_harness",
                    connection_id = %source_id,
                    event = %event_name,
                    "extension lacks tool declaration authority"
                );
                return Ok(());
            }
            if self.should_stage_extension_capabilities(source_id) {
                *self
                    .extensions
                    .pending_tool_lifecycle_declarations
                    .entry(source_id.clone())
                    .or_default() += 1;
            }
            let persist = persist_override.unwrap_or_else(|| event.defaults_to_persist());
            self.enqueue_publish_with_admission(
                Some(source_id),
                event,
                persist,
                false,
                None,
                admission,
            );
            return Ok(());
        }
        if matches!(event, Event::AgentRuntimeIndicatorsDeclared(_)) {
            let authorized = self.extensions.entries.get(source_id).is_some_and(|entry| {
                matches!(entry.kind, ClientKind::Tool | ClientKind::Core)
                    && entry.state != ExtensionState::Disconnected
            });
            if !authorized {
                tracing::warn!(
                    target: "tau_harness",
                    connection_id = %source_id,
                    event = %event_name,
                    "extension lacks agent runtime indicator declaration authority"
                );
                return Ok(());
            }
            self.handle_extension_fallback_event_with_admission(
                source_id,
                event,
                Some(false),
                admission,
            );
            return Ok(());
        }
        if matches!(
            event,
            Event::ToolProgressReported(_)
                | Event::ToolResultReported(_)
                | Event::ToolErrorReported(_)
                | Event::ToolCancelledReported(_)
        ) {
            // This is only declarative event-authority admission. Per
            // `specs/SPEC-peer-event-publication.md`, routed-call
            // validation, background suppression, and canonical publication run
            // from the committed-event consumer. Keep this path semantically
            // identical to ordinary generic Emit publication.
            let authorized = self.extensions.entries.get(source_id).is_some_and(|entry| {
                matches!(entry.kind, ClientKind::Tool | ClientKind::Core)
                    && entry.state != ExtensionState::Disconnected
            });
            if !authorized {
                tracing::warn!(
                    target: "tau_harness",
                    connection_id = %source_id,
                    event = %event_name,
                    "extension lacks tool report authority"
                );
                return Ok(());
            }
            self.handle_extension_fallback_event_with_admission(
                source_id,
                event,
                persist_override,
                admission,
            );
            return Ok(());
        }
        if matches!(
            event,
            Event::ShellCommandProgress(_) | Event::ShellCommandFinished(_)
        ) {
            // Canonical shell command state is harness-authored. Tool/Core
            // extensions publish the corresponding `_reported` observation.
            return Ok(());
        }
        if matches!(
            event,
            Event::ShellCommandProgressReported(_) | Event::ShellCommandFinishedReported(_)
        ) {
            // This is configured report-authority admission only. The committed
            // consumer revalidates the exact extension generation and routed
            // command ownership before publishing canonical shell state.
            let authorized = self.extensions.entries.get(source_id).is_some_and(|entry| {
                matches!(entry.kind, ClientKind::Tool | ClientKind::Core)
                    && entry.state != ExtensionState::Disconnected
            });
            if !authorized {
                tracing::warn!(
                    target: "tau_harness",
                    connection_id = %source_id,
                    event = %event_name,
                    "extension lacks shell command report authority"
                );
                return Ok(());
            }
            self.handle_extension_fallback_event_with_admission(
                source_id,
                event,
                persist_override,
                admission,
            );
            return Ok(());
        }
        if let Event::ToolRequest(request) = &event {
            // This is only structural and authoring-authority admission. Per
            // `specs/SPEC-peer-event-publication.md`, duplicate
            // correlation checks, pending-call bookkeeping, and registry routing
            // run from the committed-event consumer.
            let authorized = self.extensions.entries.get(source_id).is_some_and(|entry| {
                matches!(
                    entry.kind,
                    ClientKind::Provider | ClientKind::Tool | ClientKind::Core
                ) && entry.state != ExtensionState::Disconnected
            });
            if !authorized {
                tracing::warn!(
                    target: "tau_harness",
                    connection_id = %source_id,
                    event = %event_name,
                    "extension lacks tool request authority"
                );
                return Ok(());
            }
            if request.call_id.is_empty() {
                self.reject_extension_tool_request(format!(
                    "extension emitted tool request `{}` with an empty call_id; refusing to publish it",
                    request.tool_name
                ));
                return Ok(());
            }
            let persist = persist_override.unwrap_or_else(|| event.defaults_to_persist());
            self.enqueue_publish_with_admission(
                Some(source_id),
                event,
                persist,
                false,
                None,
                admission,
            );
            return Ok(());
        }
        if matches!(event, Event::ProviderModelsUpdated(_)) {
            // Canonical provider state is harness-authored. Providers publish the
            // mutable `provider.models_declared` input instead.
            return Ok(());
        }
        if matches!(
            event,
            Event::ProviderPromptSubmitted(_)
                | Event::ProviderResponseUpdated(_)
                | Event::ProviderResponseFinished(_)
                | Event::ProviderCacheMissDiagnostic(_)
                | Event::ProviderCacheRefreshFinished(_)
                | Event::AgentCacheRefreshRequested(_)
                | Event::AgentCacheRefreshCancelRequested(_)
                | Event::AgentPromptFailed(_)
                | Event::AgentPromptRejected(_)
                | Event::AgentPromptTerminated(_)
        ) {
            // Canonical provider execution facts and pre-materialization prompt
            // terminals are harness-authored. Configured providers publish only
            // corresponding `_reported` observations; correlation and terminal
            // work happen after those reports commit.
            return Ok(());
        }
        if matches!(
            event,
            Event::ProviderPromptSubmittedReported(_)
                | Event::ProviderResponseUpdatedReported(_)
                | Event::ProviderResponseFinishedReported(_)
                | Event::ProviderRetryPromptResultReported(_)
                | Event::ProviderCacheMissDiagnosticReported(_)
                | Event::ProviderCacheRefreshFinishedReported(_)
        ) {
            // This is only configured event-authority admission. Per
            // `specs/SPEC-peer-event-publication.md`, prompt ownership,
            // retry correlation, response normalization, and terminal processing
            // run from the committed-event consumer.
            let authorized = self.extensions.entries.get(source_id).is_some_and(|entry| {
                entry.kind == ClientKind::Provider && entry.state != ExtensionState::Disconnected
            });
            if !authorized {
                tracing::warn!(
                    target: "tau_harness",
                    connection_id = %source_id,
                    event = %event_name,
                    "extension lacks provider execution report authority"
                );
                return Ok(());
            }
            self.handle_extension_fallback_event_with_admission(
                source_id,
                event,
                persist_override,
                admission,
            );
            return Ok(());
        }
        if matches!(
            event,
            Event::ProviderQuotaReplaceReported(_)
                | Event::ProviderQuotaPatchReported(_)
                | Event::ProviderQuotaClearReported(_)
        ) {
            // This is only declarative event-authority admission. Per
            // `specs/SPEC-peer-event-publication.md`, provider ownership,
            // route bindings, bounds, and epoch/sequence validation run from the
            // committed-event consumer. A configured Provider's unowned payload
            // still commits as its report before downstream validation rejects it.
            let authorized = self.extensions.entries.get(source_id).is_some_and(|entry| {
                entry.kind == ClientKind::Provider && entry.state != ExtensionState::Disconnected
            });
            if !authorized {
                tracing::warn!(
                    target: "tau_harness",
                    connection_id = %source_id,
                    event = %event_name,
                    "extension lacks provider quota report authority"
                );
                return Ok(());
            }
            self.handle_extension_fallback_event_with_admission(
                source_id,
                event,
                persist_override,
                admission,
            );
            return Ok(());
        }
        if matches!(event, Event::ProviderModelsDeclared(_)) {
            // This is declarative source-aware admission, not provider-model
            // processing. `SPEC-peer-event-publication` requires the
            // accepted declaration to use ordinary interception/commit before the
            // downstream consumer derives canonical current state.
            if !self.is_provider_extension(source_id)
                || !self.accepts_provider_event_from(source_id, &event_name)
            {
                return Ok(());
            }
            if self.should_stage_extension_capabilities(source_id) {
                *self
                    .extensions
                    .pending_provider_model_declarations
                    .entry(source_id.clone())
                    .or_default() += 1;
            }
            self.enqueue_publish_with_admission(
                Some(source_id),
                event,
                false,
                false,
                None,
                admission,
            );
            return Ok(());
        }
        if matches!(event, Event::ExtPromptFragmentPublish(_)) {
            // This is only configured event-authority admission. Every
            // authenticated configured extension kind owns its source/name
            // fragment slots. Projection replacement and prompt assembly happen
            // only after ordinary interception and commit. Configured peers are
            // trusted local executables under `SECURITY.md#local-ipc-and-external-ingress`;
            // see `SPEC-prompt-fragment-declarations-and-projection`.
            let authorized = self
                .extensions
                .entries
                .get(source_id)
                .is_some_and(|entry| entry.state != ExtensionState::Disconnected);
            if !authorized {
                tracing::warn!(
                    target: "tau_harness",
                    connection_id = %source_id,
                    event = %event_name,
                    "peer lacks prompt-fragment declaration authority"
                );
                return Ok(());
            }
            if self.should_stage_extension_capabilities(source_id) {
                *self
                    .extensions
                    .pending_prompt_fragment_declarations
                    .entry(source_id.clone())
                    .or_default() += 1;
            }
            let persist = persist_override.unwrap_or_else(|| event.defaults_to_persist());
            self.enqueue_publish_with_admission(
                Some(source_id),
                event,
                persist,
                false,
                None,
                admission,
            );
            return Ok(());
        }
        if matches!(event, Event::ExtInternalPromptSubmitRequest(_)) {
            // This is configured request-authority admission only. Loaded-agent
            // validation and prompt submission happen after ordinary commit under
            // `SPEC-internal-prompt-submit-requests`.
            let authorized = self
                .extensions
                .entries
                .get(source_id)
                .is_some_and(|entry| entry.state != ExtensionState::Disconnected)
                && self
                    .bus
                    .connection(source_id)
                    .is_some_and(|connection| connection.origin != ConnectionOrigin::Socket);
            if !authorized {
                tracing::warn!(
                    target: "tau_harness",
                    connection_id = %source_id,
                    event = %event_name,
                    "peer lacks internal-prompt request authority"
                );
                return Ok(());
            }
            let persist = persist_override.unwrap_or_else(|| event.defaults_to_persist());
            self.enqueue_publish_with_admission(
                Some(source_id),
                event,
                persist,
                false,
                None,
                admission,
            );
            return Ok(());
        }
        if matches!(event, Event::StartAgentRequest(_)) {
            // This is configured request-authority admission only. Role and
            // parent validation, duplicate rebinding, acceptance/result routing,
            // and agent creation happen after ordinary commit.
            let authorized = self
                .extensions
                .entries
                .get(source_id)
                .is_some_and(|entry| entry.state != ExtensionState::Disconnected)
                && self
                    .bus
                    .connection(source_id)
                    .is_some_and(|connection| connection.origin != ConnectionOrigin::Socket);
            if !authorized {
                tracing::warn!(
                    target: "tau_harness",
                    connection_id = %source_id,
                    event = %event_name,
                    "peer lacks start-agent request authority"
                );
                return Ok(());
            }
            let persist = persist_override.unwrap_or_else(|| event.defaults_to_persist());
            self.enqueue_publish_with_admission(
                Some(source_id),
                event,
                persist,
                false,
                None,
                admission,
            );
            return Ok(());
        }
        if matches!(event, Event::Osc1337SetUserVar(_) | Event::TermBell(_)) {
            // Terminal-output events are live side-effect requests. This is
            // configured event-authority admission only; the terminal UI reacts
            // to the event after ordinary interception, commit, and broadcast.
            let authorized = self
                .extensions
                .entries
                .get(source_id)
                .is_some_and(|entry| entry.state != ExtensionState::Disconnected)
                && self
                    .bus
                    .connection(source_id)
                    .is_some_and(|connection| connection.origin != ConnectionOrigin::Socket);
            if !authorized {
                tracing::warn!(
                    target: "tau_harness",
                    connection_id = %source_id,
                    event = %event_name,
                    "peer lacks terminal-output event authority"
                );
                return Ok(());
            }
            let persist = persist_override.unwrap_or_else(|| event.defaults_to_persist());
            self.enqueue_publish_with_admission(
                Some(source_id),
                event,
                persist,
                false,
                None,
                admission,
            );
            return Ok(());
        }
        if matches!(event, Event::ExtensionEvent(_)) {
            // Custom events assert only extension-owned names and payloads.
            // Ordinary subscribers consume them after generic interception,
            // commit, and broadcast; no harness semantic work happens here.
            let authorized = self
                .extensions
                .entries
                .get(source_id)
                .is_some_and(|entry| entry.state != ExtensionState::Disconnected)
                && self
                    .bus
                    .connection(source_id)
                    .is_some_and(|connection| connection.origin != ConnectionOrigin::Socket);
            if !authorized {
                tracing::warn!(
                    target: "tau_harness",
                    connection_id = %source_id,
                    event = %event_name,
                    "peer lacks custom-event authority"
                );
                return Ok(());
            }
            let persist = persist_override.unwrap_or_else(|| event.defaults_to_persist());
            self.enqueue_publish_with_admission(
                Some(source_id),
                event,
                persist,
                false,
                None,
                admission,
            );
            return Ok(());
        }
        if matches!(
            event,
            Event::ExtensionContextProviderRegister(_)
                | Event::ExtensionAgentDiscoverySnapshotDeclared(_)
                | Event::ExtAgentContextPublish(_)
                | Event::ExtensionContextReady(_)
        ) {
            // This is configured event-authority admission only. Registration,
            // context projection, and readiness release happen after ordinary
            // commit under `SPEC-per-agent-context-declarations-and-readiness`.
            let authorized = self
                .extensions
                .entries
                .get(source_id)
                .is_some_and(|entry| entry.state != ExtensionState::Disconnected)
                && self
                    .bus
                    .connection(source_id)
                    .is_some_and(|connection| connection.origin != ConnectionOrigin::Socket);
            if !authorized {
                tracing::warn!(
                    target: "tau_harness",
                    connection_id = %source_id,
                    event = %event_name,
                    "peer lacks per-agent context event authority"
                );
                return Ok(());
            }
            let declaration = matches!(
                event,
                Event::ExtensionContextProviderRegister(_)
                    | Event::ExtensionAgentDiscoverySnapshotDeclared(_)
                    | Event::ExtAgentContextPublish(_)
            );
            if declaration && self.should_stage_extension_capabilities(source_id) {
                *self
                    .extensions
                    .pending_agent_context_declarations
                    .entry(source_id.clone())
                    .or_default() += 1;
            }
            let persist = persist_override.unwrap_or_else(|| event.defaults_to_persist());
            self.enqueue_publish_with_admission(
                Some(source_id),
                event,
                persist,
                false,
                None,
                admission,
            );
            return Ok(());
        }
        if matches!(
            event,
            Event::ExtensionSessionContextProviderRegister(_)
                | Event::ExtensionSessionDiscoverySnapshotDeclared(_)
                | Event::ExtensionSessionContextReady(_)
        ) {
            // This is configured event-authority admission only. Registration,
            // discovery projection, diagnostics, instruction injection, and
            // readiness release happen after ordinary commit under
            // `SPEC-session-discovery-declarations-and-readiness`.
            let authorized = self
                .extensions
                .entries
                .get(source_id)
                .is_some_and(|entry| entry.state != ExtensionState::Disconnected)
                && self
                    .bus
                    .connection(source_id)
                    .is_some_and(|connection| connection.origin != ConnectionOrigin::Socket);
            if !authorized {
                tracing::warn!(
                    target: "tau_harness",
                    connection_id = %source_id,
                    event = %event_name,
                    "peer lacks session-discovery event authority"
                );
                return Ok(());
            }
            let declaration = matches!(
                event,
                Event::ExtensionSessionContextProviderRegister(_)
                    | Event::ExtensionSessionDiscoverySnapshotDeclared(_)
            );
            if declaration && self.should_stage_extension_capabilities(source_id) {
                *self
                    .extensions
                    .pending_session_discovery_declarations
                    .entry(source_id.clone())
                    .or_default() += 1;
            }
            let persist = persist_override.unwrap_or_else(|| event.defaults_to_persist());
            self.enqueue_publish_with_admission(
                Some(source_id),
                event,
                persist,
                false,
                None,
                admission,
            );
            return Ok(());
        }
        if matches!(
            event,
            Event::AgentMetadataSetRequest(_) | Event::AgentMetadataUnsetRequest(_)
        ) {
            // Metadata mutation is configured extension authority only. The
            // committed request is validated and canonicalized downstream.
            let authorized = self
                .extensions
                .entries
                .get(source_id)
                .is_some_and(|entry| entry.state != ExtensionState::Disconnected)
                && self
                    .bus
                    .connection(source_id)
                    .is_some_and(|connection| connection.origin != ConnectionOrigin::Socket);
            if !authorized {
                tracing::warn!(
                    target: "tau_harness",
                    connection_id = %source_id,
                    event = %event_name,
                    "peer lacks agent metadata request authority"
                );
                return Ok(());
            }
            let persist = persist_override.unwrap_or_else(|| event.defaults_to_persist());
            self.enqueue_publish_with_admission(
                Some(source_id),
                event,
                persist,
                false,
                None,
                admission,
            );
            return Ok(());
        }
        if event_name.category() == &tau_proto::EventCategory::Provider
            && !self.accepts_provider_event_from(source_id, &event_name)
        {
            return Ok(());
        }

        let Some(event) = self.handle_extension_tool_terminal_event(source_id, event) else {
            return Ok(());
        };
        self.handle_extension_fallback_event_with_admission(
            source_id,
            event,
            persist_override,
            admission,
        );
        Ok(())
    }

    /// Return whether one configured-peer event updates process-global state
    /// that remains valid across a session rollover for the same
    /// connection/instance.
    pub(super) fn peer_event_semantics_survive_rollover(event: &Event) -> bool {
        matches!(
            event,
            Event::ToolRegistrationDeclared(_)
                | Event::ToolUnregistrationDeclared(_)
                | Event::ExtPromptFragmentPublish(_)
                | Event::ProviderModelsDeclared(_)
                | Event::ProviderQuotaReplaceReported(_)
                | Event::ProviderQuotaPatchReported(_)
                | Event::ProviderQuotaClearReported(_)
                | Event::ActionSchemaDeclared(_)
        )
    }

    /// Process one peer-authored event only after the generic publication path
    /// has committed and broadcast it.
    ///
    /// Keep semantic work out of `HarnessInputMessage::Emit` intake. This is
    /// the downstream boundary required by
    /// `SPEC-peer-event-publication`; adding a new `Emit` special case
    /// in `handle_extension_message` would bypass interception and recreate the
    /// architectural problem that decision prohibits.
    pub(super) fn process_committed_peer_event(
        &mut self,
        source: Option<&tau_proto::ConnectionId>,
        peer_context: &interception::PeerPublicationContext,
        event: &Event,
    ) {
        if !Self::peer_event_semantics_survive_rollover(event)
            && peer_context.extension.as_ref().is_some_and(|extension| {
                extension.admission.session_id != self.current_session_id
                    || extension.admission.session_generation != self.current_session_generation
            })
        {
            // The raw event has already committed and remains observable. A
            // rollover generation boundary suppresses session-bound downstream
            // semantics. Explicitly process-global declarations and current-state
            // reports continue below under exact connection/instance checks.
            self.discard_peer_activation_reservation(peer_context);
            return;
        }
        if let Event::AgentRuntimeIndicatorsDeclared(declaration) = event {
            self.process_committed_agent_runtime_indicators(peer_context, declaration);
            return;
        }
        if let Some(publisher) = peer_context.extension.as_ref().map(|extension| {
            tau_proto::MessagePublisherId::from_extension_name(&extension.publisher)
        }) && let Some(canonical) = event.clone().into_stamped_canonical_message_fact(publisher)
        {
            self.enqueue_publish(
                Some(crate::harness::harness_connection_id()),
                canonical,
                true,
                true,
                None,
            );
            return;
        }
        if matches!(
            event,
            Event::ToolRegistrationDeclared(_) | Event::ToolUnregistrationDeclared(_)
        ) {
            self.process_committed_tool_declaration(peer_context, event);
            return;
        }
        if matches!(
            event,
            Event::ActionSchemaDeclared(_)
                | Event::ActionResultReported(_)
                | Event::ActionErrorReported(_)
        ) {
            self.process_committed_action_event(peer_context, event);
            return;
        }
        if let Event::ToolRequest(request) = event {
            self.process_committed_tool_request(peer_context, request);
            return;
        }
        if let Event::ToolProgressReported(progress) = event {
            self.process_committed_tool_progress_report(peer_context, progress);
            return;
        }
        if matches!(
            event,
            Event::ToolResultReported(_)
                | Event::ToolErrorReported(_)
                | Event::ToolCancelledReported(_)
        ) {
            self.process_committed_tool_terminal_report(peer_context, event);
            return;
        }
        if matches!(
            event,
            Event::ShellCommandProgressReported(_) | Event::ShellCommandFinishedReported(_)
        ) {
            self.process_committed_shell_command_report(peer_context, event);
            return;
        }
        if matches!(
            event,
            Event::ProviderQuotaReplaceReported(_)
                | Event::ProviderQuotaPatchReported(_)
                | Event::ProviderQuotaClearReported(_)
        ) {
            self.process_committed_provider_quota_report(peer_context, event);
            return;
        }
        if matches!(
            event,
            Event::ProviderPromptSubmittedReported(_)
                | Event::ProviderResponseUpdatedReported(_)
                | Event::ProviderResponseFinishedReported(_)
                | Event::ProviderRetryPromptResultReported(_)
                | Event::ProviderCacheMissDiagnosticReported(_)
                | Event::ProviderCacheRefreshFinishedReported(_)
        ) {
            self.process_committed_provider_execution_report(peer_context, event);
            return;
        }
        if matches!(event, Event::ExtPromptFragmentPublish(_)) {
            self.process_committed_prompt_fragment(peer_context, event);
            return;
        }
        if let Event::ExtInternalPromptSubmitRequest(request) = event {
            self.process_committed_internal_prompt_submit_request(peer_context, request);
            return;
        }
        if let Event::StartAgentRequest(request) = event {
            self.process_committed_start_agent_request(peer_context, request);
            return;
        }
        if matches!(
            event,
            Event::AgentMetadataSetRequest(_) | Event::AgentMetadataUnsetRequest(_)
        ) {
            self.process_committed_agent_metadata_request(source, peer_context, event);
            return;
        }
        if matches!(
            event,
            Event::ExtensionContextProviderRegister(_)
                | Event::ExtensionAgentDiscoverySnapshotDeclared(_)
                | Event::ExtAgentContextPublish(_)
                | Event::ExtensionContextReady(_)
        ) {
            self.process_committed_agent_context_event(peer_context, event);
            return;
        }
        if matches!(
            event,
            Event::ExtensionSessionContextProviderRegister(_)
                | Event::ExtensionSessionDiscoverySnapshotDeclared(_)
                | Event::ExtensionSessionContextReady(_)
        ) {
            self.process_committed_session_discovery_event(peer_context, event);
            return;
        }
        let Event::ProviderModelsDeclared(declaration) = event else {
            return;
        };
        let Some(extension) = peer_context
            .extension
            .as_ref()
            .filter(|extension| extension.kind == ClientKind::Provider)
        else {
            return;
        };
        let source_id = &extension.source;
        let publisher_extension_id = extension.publisher.clone();
        let source_is_current = self.extensions.entries.get(source_id).is_some_and(|entry| {
            entry.connection_id == extension.source
                && entry.instance_id == extension.instance_id
                && entry.name == extension.publisher
                && entry.kind == ClientKind::Provider
                && entry.state != ExtensionState::Disconnected
        });
        if !source_is_current {
            // The declaration still committed with its captured publisher
            // envelope, but a disconnected or replaced provider generation must
            // not recreate stale routes or consume the new generation's
            // activation reservation.
            return;
        }
        if let Some(reservation) = extension.activation_reservation
            && !self.reaccount_activation_reservation(source_id, reservation, event)
        {
            self.finish_pending_provider_model_declaration(source_id);
            return;
        }
        if self.should_stage_extension_capabilities(source_id)
            && extension.activation_reservation.is_some()
        {
            self.stage_provider_models_update(
                source_id,
                tau_proto::ProviderModelsUpdated {
                    publisher_extension_id,
                    models: declaration.models.clone(),
                },
            );
        } else {
            self.publish_provider_models_update(
                source_id,
                publisher_extension_id,
                declaration.clone(),
            );
        }
        if extension.activation_reservation.is_some() {
            self.finish_pending_provider_model_declaration(source_id);
        }
    }

    /// Validate and submit one internal-prompt request only after generic peer
    /// publication commits for the exact configured connection generation.
    pub(super) fn process_committed_internal_prompt_submit_request(
        &mut self,
        peer_context: &interception::PeerPublicationContext,
        request: &tau_proto::ExtInternalPromptSubmitRequest,
    ) {
        let Some(extension) = peer_context.extension.as_ref() else {
            return;
        };
        let source_id = &extension.source;
        let source_is_current = self.extensions.entries.get(source_id).is_some_and(|entry| {
            extension.admission.session_id == self.current_session_id
                && extension.admission.session_generation == self.current_session_generation
                && entry.connection_id == extension.source
                && entry.instance_id == extension.instance_id
                && entry.name == extension.publisher
                && entry.kind == extension.kind
                && entry.state != ExtensionState::Disconnected
        });
        if !source_is_current {
            self.discard_peer_activation_reservation(peer_context);
            return;
        }
        if let Err(error) =
            self.handle_extension_internal_prompt_submit_request(&extension.publisher, request)
        {
            self.publication.pending_error.get_or_insert(error);
        }
    }

    /// Process one start-agent request only after generic peer publication
    /// commits for the exact configured connection generation.
    pub(super) fn process_committed_start_agent_request(
        &mut self,
        peer_context: &interception::PeerPublicationContext,
        request: &tau_proto::StartAgentRequest,
    ) {
        let Some(extension) = peer_context.extension.as_ref() else {
            return;
        };
        let source_id = &extension.source;
        let source_is_current = self.extensions.entries.get(source_id).is_some_and(|entry| {
            extension.admission.session_id == self.current_session_id
                && extension.admission.session_generation == self.current_session_generation
                && entry.connection_id == extension.source
                && entry.instance_id == extension.instance_id
                && entry.name == extension.publisher
                && entry.kind == extension.kind
                && entry.state != ExtensionState::Disconnected
        });
        if !source_is_current {
            return;
        }
        if let Err(error) = self.handle_start_agent_request(source_id, request.clone()) {
            self.publication.pending_error.get_or_insert(error);
        }
    }

    /// Validate one committed metadata request and publish its canonical fact.
    ///
    /// Extension requests retain their exact configured connection generation
    /// through interception. UI requests retain their run-local socket
    /// connection id and must still come from an attached UI when
    /// downstream processing runs. Invalid or stale requests return without a
    /// canonical successor; canonical store failure likewise prevents an echo.
    pub(super) fn process_committed_agent_metadata_request(
        &mut self,
        source: Option<&tau_proto::ConnectionId>,
        peer_context: &interception::PeerPublicationContext,
        event: &Event,
    ) {
        let source_is_current = if let Some(extension) = peer_context.extension.as_ref() {
            self.extensions
                .entries
                .get(&extension.source)
                .is_some_and(|entry| {
                    entry.connection_id == extension.source
                        && entry.instance_id == extension.instance_id
                        && entry.name == extension.publisher
                        && entry.kind == extension.kind
                        && entry.state != ExtensionState::Disconnected
                })
                && self
                    .bus
                    .connection(&extension.source)
                    .is_some_and(|connection| connection.origin != ConnectionOrigin::Socket)
        } else {
            source.is_some_and(|source_id| self.is_attached_socket_ui(source_id))
        };
        if !source_is_current {
            return;
        }
        let canonical = match event {
            Event::AgentMetadataSetRequest(set) => {
                if self.validate_agent_metadata_set(set).is_err() {
                    return;
                }
                Event::AgentMetadataSet(set.clone())
            }
            Event::AgentMetadataUnsetRequest(unset) => {
                if self.validate_agent_metadata_unset(unset).is_err() {
                    return;
                }
                Event::AgentMetadataUnset(unset.clone())
            }
            _ => return,
        };
        self.enqueue_publish(
            Some(crate::harness::harness_connection_id()),
            canonical,
            true,
            false,
            None,
        );
    }

    /// Apply one per-agent context declaration, value, or readiness
    /// acknowledgement only after it commits for the exact configured
    /// connection generation.
    pub(super) fn process_committed_agent_context_event(
        &mut self,
        peer_context: &interception::PeerPublicationContext,
        event: &Event,
    ) {
        let Some(extension) = peer_context.extension.as_ref() else {
            return;
        };
        let source_id = &extension.source;
        let source_is_current = self.extensions.entries.get(source_id).is_some_and(|entry| {
            extension.admission.session_id == self.current_session_id
                && extension.admission.session_generation == self.current_session_generation
                && entry.connection_id == extension.source
                && entry.instance_id == extension.instance_id
                && entry.name == extension.publisher
                && entry.kind == extension.kind
                && entry.state != ExtensionState::Disconnected
        });
        if !source_is_current {
            self.discard_peer_activation_reservation(peer_context);
            return;
        }
        if let Some(reservation) = extension.activation_reservation
            && !self.reaccount_activation_reservation(source_id, reservation, event)
        {
            self.finish_pending_agent_context_declaration(source_id);
            return;
        }

        match event {
            Event::ExtensionContextProviderRegister(_) => {
                if self.should_stage_extension_capabilities(source_id) {
                    self.stage_agent_context_provider_register(
                        source_id,
                        extension.admission.clone(),
                    );
                } else {
                    self.apply_agent_context_provider_registration(source_id);
                }
            }
            Event::ExtAgentContextPublish(publish) => {
                if self.should_stage_extension_capabilities(source_id) {
                    self.stage_agent_context_publish(
                        source_id,
                        publish.clone(),
                        extension.admission.clone(),
                    );
                } else {
                    self.apply_agent_context_publish(source_id, publish.clone());
                }
            }
            Event::ExtensionAgentDiscoverySnapshotDeclared(snapshot) => {
                if self.should_stage_extension_capabilities(source_id) {
                    self.stage_agent_discovery_snapshot(
                        source_id,
                        snapshot.clone(),
                        extension.admission.clone(),
                    );
                } else {
                    self.apply_agent_discovery_snapshot(source_id, snapshot.clone());
                }
            }
            Event::ExtensionContextReady(ready) => {
                if let Err(error) = self.apply_extension_context_ready(source_id, ready.clone()) {
                    self.publication.pending_error.get_or_insert(error);
                }
            }
            _ => unreachable!("caller filters per-agent context events"),
        }

        if extension.activation_reservation.is_some() {
            self.finish_pending_agent_context_declaration(source_id);
        }
    }

    /// Route one peer request only after its ordinary generic commit.
    ///
    /// The request has already passed interception, persistence when selected,
    /// debug recording, and broadcast. Correlation checks, pending-call
    /// mutation, and registry routing intentionally remain downstream under
    /// `specs/SPEC-peer-event-publication.md`.
    pub(super) fn process_committed_tool_request(
        &mut self,
        peer_context: &interception::PeerPublicationContext,
        request: &ToolRequest,
    ) {
        let Some(extension) = peer_context.extension.as_ref().filter(|extension| {
            matches!(
                extension.kind,
                ClientKind::Provider | ClientKind::Tool | ClientKind::Core
            )
        }) else {
            return;
        };
        let source_is_current =
            self.extensions
                .entries
                .get(&extension.source)
                .is_some_and(|entry| {
                    entry.connection_id == extension.source
                        && entry.instance_id == extension.instance_id
                        && entry.name == extension.publisher
                        && entry.kind == extension.kind
                        && entry.state != ExtensionState::Disconnected
                });
        if !source_is_current {
            // The request remains a committed observation, but a parked stale
            // generation cannot route work or alter replacement-generation state.
            return;
        }
        if let Some(message) = self.extension_tool_request_rejection(request) {
            self.reject_extension_tool_request(message);
            return;
        }

        self.track_extension_tool_request_metadata(request);
        let turn_categories = self
            .registry
            .resolve_provider(request.tool_name.as_str())
            .map_or_else(ToolTurnCategories::default, |provider| {
                ToolTurnCategories::from_tags(&provider.tool.tags)
            });
        match self.registry.route_tool_request(request.clone()) {
            Ok(route) => {
                let Some(cid) = self
                    .agent_registry
                    .agent_routes
                    .get(request.agent_id.as_str())
                    .filter(|cid| self.agent_registry.agents.contains_key(*cid))
                    .cloned()
                else {
                    self.reject_peer_tool_request(
                        request.clone(),
                        request.tool_name.clone(),
                        format!(
                            "cannot invoke tool `{}` for unavailable agent `{}`",
                            request.tool_name, request.agent_id
                        ),
                    );
                    return;
                };
                self.tool_runtime
                    .peer_internal_tool_agents
                    .insert(request.call_id.clone(), cid.clone());
                self.tool_runtime.tool_turn.record_unqueued_in_flight(
                    cid.clone(),
                    request.call_id.clone(),
                    turn_categories,
                );
                self.bump_tools_started_for(&cid);
                match &route.target {
                    ToolRouteTarget::Internal => {
                        // Configured extensions are trusted local executables; see
                        // `SECURITY.md#local-ipc-and-external-ingress`. Their
                        // payload agent id supplies ordinary request correlation,
                        // while the harness still requires a live loaded route.
                        self.record_wait_tool_request(&request.call_id);
                    }
                    ToolRouteTarget::Extension(provider_connection_id) => {
                        // Establish terminal-report authority before the selected
                        // tool can observe `tool.started` and immediately answer.
                        self.ensure_tool_started_subscription(provider_connection_id);
                        self.tool_runtime
                            .pending_tool_providers
                            .insert(request.call_id.clone(), provider_connection_id.clone());
                        self.tool_runtime
                            .peer_tool_requests
                            .insert(request.call_id.clone());
                    }
                }
                let event = Event::ToolStarted(route.invoke);
                self.publish_event(Some(crate::harness::harness_connection_id()), event);
            }
            Err(ToolRouteError::NoProvider { tool_name }) => {
                self.reject_unroutable_extension_tool_request(request.clone(), tool_name);
            }
            Err(error) => {
                self.publication.pending_error = Some(HarnessError::ToolRoute(error));
            }
        }
    }

    /// Apply one committed provider-quota report from the still-current
    /// captured provider generation.
    ///
    /// The report has already passed ordinary interception, commit, and
    /// broadcast. Provider ownership, route bindings, bounds, and
    /// epoch/sequence transitions intentionally remain downstream under
    /// `SPEC-peer-event-publication`.
    pub(super) fn process_committed_provider_quota_report(
        &mut self,
        peer_context: &interception::PeerPublicationContext,
        event: &Event,
    ) {
        let Some(extension) = peer_context
            .extension
            .as_ref()
            .filter(|extension| extension.kind == ClientKind::Provider)
        else {
            return;
        };
        let source_is_current =
            self.extensions
                .entries
                .get(&extension.source)
                .is_some_and(|entry| {
                    entry.connection_id == extension.source
                        && entry.instance_id == extension.instance_id
                        && entry.name == extension.publisher
                        && entry.kind == ClientKind::Provider
                        && entry.state != ExtensionState::Disconnected
                });
        if !source_is_current {
            // A parked stale-generation report remains a committed observation,
            // but it cannot mutate the replacement provider's current state.
            return;
        }
        match event {
            Event::ProviderQuotaReplaceReported(replace) => {
                self.handle_provider_quota_replace(&extension.source, replace.clone());
            }
            Event::ProviderQuotaPatchReported(patch) => {
                self.handle_provider_quota_patch(&extension.source, patch.clone());
            }
            Event::ProviderQuotaClearReported(clear) => {
                self.handle_provider_quota_clear(&extension.source, clear.clone());
            }
            _ => unreachable!("caller filters provider quota reports"),
        }
    }

    /// Apply one committed provider-execution report from the still-current
    /// captured provider generation.
    ///
    /// The report has already passed ordinary interception, commit, and
    /// broadcast. Prompt ownership, retry correlation, response normalization,
    /// and terminal response processing intentionally remain downstream under
    /// `specs/SPEC-peer-event-publication.md`.
    pub(super) fn process_committed_provider_execution_report(
        &mut self,
        peer_context: &interception::PeerPublicationContext,
        event: &Event,
    ) {
        let Some(extension) = peer_context
            .extension
            .as_ref()
            .filter(|extension| extension.kind == ClientKind::Provider)
        else {
            return;
        };
        let source_is_current =
            self.extensions
                .entries
                .get(&extension.source)
                .is_some_and(|entry| {
                    entry.connection_id == extension.source
                        && entry.instance_id == extension.instance_id
                        && entry.name == extension.publisher
                        && entry.kind == ClientKind::Provider
                        && entry.state != ExtensionState::Disconnected
                });
        if !source_is_current {
            // The observation remains committed, but a parked stale generation
            // cannot mutate prompt, retry, recovery, tool, or turn state.
            return;
        }
        let source_id = &extension.source;
        match event {
            Event::ProviderRetryPromptResultReported(result) => {
                self.process_provider_retry_prompt_result_report(source_id, result);
            }
            Event::ProviderPromptSubmittedReported(submitted) => {
                self.process_provider_prompt_submitted_report(source_id, submitted);
            }
            Event::ProviderResponseUpdatedReported(updated) => {
                self.process_provider_response_updated_report(source_id, updated);
            }
            Event::ProviderResponseFinishedReported(response) => {
                self.process_provider_response_finished_report(source_id, response);
            }
            Event::ProviderCacheMissDiagnosticReported(diagnostic) => {
                self.process_provider_cache_miss_diagnostic_report(source_id, diagnostic);
            }
            Event::ProviderCacheRefreshFinishedReported(finished) => {
                if self
                    .provider_runtime.cache_residency
                    .finish(source_id, &finished.refresh_id)
                {
                    self.publish_event(
                        Some(crate::harness::harness_connection_id()),
                        Event::ProviderCacheRefreshFinished(finished.clone()),
                    );
                }
            }
            _ => unreachable!("caller filters provider execution reports"),
        }
    }

    pub(super) fn process_provider_retry_prompt_result_report(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        result: &tau_proto::ProviderRetryPromptResult,
    ) {
        let Some(pending) = self
            .ui_runtime
            .pending_retry_prompts
            .get(&result.request_id)
            .cloned()
        else {
            return;
        };
        if pending.provider_connection_id != *source_id
            || pending.agent_prompt_id != result.agent_prompt_id
        {
            tracing::warn!(
                target: "tau_harness",
                source_id = %source_id,
                agent_prompt_id = %result.agent_prompt_id,
                "discarding mismatched provider retry result report"
            );
            return;
        }
        if self.agent_is_ephemeral(&pending.target_agent_id) {
            self.ephemeral_provider_retry_requests
                .insert(result.request_id.clone());
        }
        self.ui_runtime
            .pending_retry_prompts
            .remove(&result.request_id);
        let message = match result.status {
            tau_proto::RetryPromptStatus::Accepted => {
                format!("Retrying agent {} now.", pending.target_label)
            }
            tau_proto::RetryPromptStatus::NotParked => format!(
                "No delayed provider retry is waiting for agent {}; it may already be running.",
                pending.target_label
            ),
        };
        let _ = self.bus.send_to(
            &pending.requester_client_id,
            Some(crate::harness::harness_connection_id()),
            HarnessOutputMessage::deliver(Event::UiRetryPromptResult(
                tau_proto::UiRetryPromptResult {
                    request_id: pending.ui_request_id,
                    target_agent_id: Some(pending.target_agent_id),
                    target_label: pending.target_label,
                    status: Some(result.status),
                    message,
                },
            )),
        );
    }

    pub(super) fn process_provider_prompt_submitted_report(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        submitted: &tau_proto::ProviderPromptSubmitted,
    ) {
        if !self.canceled_prompts.contains(&submitted.agent_prompt_id)
            && self.provider_prompt_owner_matches(
                source_id,
                &submitted.agent_prompt_id,
                tau_proto::EventName::PROVIDER_PROMPT_SUBMITTED_REPORTED,
            )
        {
            self.publish_event(
                Some(crate::harness::harness_connection_id()),
                Event::ProviderPromptSubmitted(submitted.clone()),
            );
        }
    }

    pub(super) fn process_provider_response_updated_report(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        updated: &tau_proto::ProviderResponseUpdated,
    ) {
        if self.canceled_prompts.contains(&updated.agent_prompt_id)
            || !self.provider_prompt_owner_matches(
                source_id,
                &updated.agent_prompt_id,
                tau_proto::EventName::PROVIDER_RESPONSE_UPDATED_REPORTED,
            )
        {
            return;
        }
        let Some(agent_id) = self.agent_id_for_prompt(&updated.agent_prompt_id) else {
            return;
        };
        let mut updated = updated.clone();
        updated.agent_id = agent_id;
        if !updated.deltas.is_empty() {
            self.prompt_semantic_output
                .insert(updated.agent_prompt_id.clone());
        }
        self.enrich_provider_response_updated_compaction(&mut updated);
        if let Some(retry) = updated
            .status
            .as_ref()
            .and_then(|status| status.retry.clone())
            && !self
                .agent_registry
                .agents
                .get(&updated.agent_id)
                .is_some_and(|agent| agent.lifecycle_notification_only_turn)
            && let Some(public_id) = self.ensure_agent_id_for_agent(&updated.agent_id)
        {
            let turn_generation = self
                .agent_registry
                .agents
                .get(&updated.agent_id)
                .map_or(0, |agent| agent.turn_generation);
            self.update_agent_watch_provider_status(
                &public_id,
                tau_proto::AgentWatchProviderStatusNotification {
                    session_id: self.current_session_id.clone(),
                    subscription_id: String::new(),
                    turn_generation,
                    agent_prompt_id: updated.agent_prompt_id.clone(),
                    state: tau_proto::AgentWatchProviderState::Retrying {
                        category: watch_category_for_retry(retry.category),
                        attempt: retry.attempt,
                        next_retry_delay_secs: retry.next_retry_delay_secs,
                    },
                    initial: false,
                },
            );
        }
        if provider_response_update_has_public_content(&updated) {
            self.publish_event(
                Some(crate::harness::harness_connection_id()),
                Event::ProviderResponseUpdated(updated),
            );
        }
    }

    pub(super) fn process_provider_response_finished_report(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        response: &tau_proto::ProviderResponseFinished,
    ) {
        if self.provider_prompt_owner_matches(
            source_id,
            &response.agent_prompt_id,
            tau_proto::EventName::PROVIDER_RESPONSE_FINISHED_REPORTED,
        ) {
            let result = self.with_derived_publish_source(
                Some(crate::harness::harness_connection_id().clone()),
                |harness| {
                    harness.handle_provider_response_finished_from(
                        Some(crate::harness::harness_connection_id()),
                        response.clone(),
                    )
                },
            );
            if let Err(error) = result {
                self.publication.pending_error.get_or_insert(error);
            }
        }
    }

    pub(super) fn process_provider_cache_miss_diagnostic_report(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        diagnostic: &tau_proto::ProviderCacheMissDiagnostic,
    ) {
        if self.provider_prompt_owner_matches(
            source_id,
            &diagnostic.agent_prompt_id,
            tau_proto::EventName::PROVIDER_CACHE_MISS_DIAGNOSTIC_REPORTED,
        ) {
            self.publish_event(
                Some(crate::harness::harness_connection_id()),
                Event::ProviderCacheMissDiagnostic(diagnostic.clone()),
            );
        }
    }

    /// Validate one committed peer progress observation and publish its
    /// canonical harness-authored fact.
    ///
    /// This method is intentionally downstream of generic `Emit` commit under
    /// `specs/SPEC-peer-event-publication.md`. Interception
    /// replacements must reach this point before routed-call ownership and
    /// background suppression are evaluated.
    pub(super) fn process_committed_tool_progress_report(
        &mut self,
        peer_context: &interception::PeerPublicationContext,
        progress: &tau_proto::ToolProgress,
    ) {
        let Some(extension) = peer_context
            .extension
            .as_ref()
            .filter(|extension| matches!(extension.kind, ClientKind::Tool | ClientKind::Core))
        else {
            return;
        };
        let source_is_current =
            self.extensions
                .entries
                .get(&extension.source)
                .is_some_and(|entry| {
                    entry.instance_id == extension.instance_id
                        && entry.name == extension.publisher
                        && matches!(entry.kind, ClientKind::Tool | ClientKind::Core)
                        && entry.state != ExtensionState::Disconnected
                });
        let source_owns_route = self
            .tool_runtime
            .pending_tool_providers
            .get(&progress.call_id)
            .is_some_and(|source| source == &extension.source);
        if !source_is_current
            || !(self
                .tool_runtime
                .tool_agents
                .contains_key(&progress.call_id)
                || self
                    .tool_runtime
                    .peer_tool_requests
                    .contains(&progress.call_id))
            || !source_owns_route
            || self
                .tool_runtime
                .tool_turn
                .is_backgrounded(&progress.call_id)
        {
            return;
        }
        let mut progress = progress.clone();
        if let Some(tool) = self.tool_runtime.pending_tools.get(&progress.call_id) {
            progress.tool_name = tool.name.clone();
        }
        self.enqueue_publish(
            Some(crate::harness::harness_connection_id()),
            Event::ToolProgress(progress),
            false,
            true,
            None,
        );
    }

    /// Validate one committed peer terminal report and apply the existing
    /// terminal completion path.
    ///
    /// The report has already passed generic interception and commit. This
    /// method revalidates the immutable captured extension generation and
    /// exact current routed-call owner before any terminal state mutation
    /// or canonical publication. Same-name interception replacements
    /// therefore rerun the full validation.
    pub(super) fn process_committed_tool_terminal_report(
        &mut self,
        peer_context: &interception::PeerPublicationContext,
        event: &Event,
    ) {
        let Some(extension) = peer_context
            .extension
            .as_ref()
            .filter(|extension| matches!(extension.kind, ClientKind::Tool | ClientKind::Core))
        else {
            return;
        };
        let source_is_current =
            self.extensions
                .entries
                .get(&extension.source)
                .is_some_and(|entry| {
                    entry.instance_id == extension.instance_id
                        && entry.name == extension.publisher
                        && matches!(entry.kind, ClientKind::Tool | ClientKind::Core)
                        && entry.state != ExtensionState::Disconnected
                });
        let call_id = match event {
            Event::ToolResultReported(result) => &result.call_id,
            Event::ToolErrorReported(error) => &error.call_id,
            Event::ToolCancelledReported(cancelled) => &cancelled.call_id,
            _ => unreachable!("caller filters terminal tool reports"),
        };
        let source_owns_route = self
            .tool_runtime
            .pending_tool_providers
            .get(call_id)
            .is_some_and(|source| source == &extension.source);
        if !source_is_current
            || !(self.tool_runtime.tool_agents.contains_key(call_id)
                || self.tool_runtime.peer_tool_requests.contains(call_id))
            || !source_owns_route
        {
            return;
        }
        let source_id = &extension.source;
        match event {
            Event::ToolResultReported(result) => {
                self.handle_extension_tool_result(source_id, result.clone());
            }
            Event::ToolErrorReported(error) => {
                self.handle_extension_tool_error(source_id, error.clone());
            }
            Event::ToolCancelledReported(cancelled) => {
                self.handle_extension_tool_cancelled(source_id, cancelled.clone());
            }
            _ => unreachable!("caller filters terminal tool reports"),
        }
    }

    /// Apply registration, discovery projection, derived facts, or readiness
    /// only after one session-discovery event commits for its exact
    /// configured connection generation.
    pub(super) fn process_committed_session_discovery_event(
        &mut self,
        peer_context: &interception::PeerPublicationContext,
        event: &Event,
    ) {
        let Some(extension) = peer_context.extension.as_ref() else {
            return;
        };
        let source_id = &extension.source;
        let source_is_current = self.extensions.entries.get(source_id).is_some_and(|entry| {
            extension.admission.session_id == self.current_session_id
                && extension.admission.session_generation == self.current_session_generation
                && entry.connection_id == extension.source
                && entry.instance_id == extension.instance_id
                && entry.name == extension.publisher
                && entry.kind == extension.kind
                && entry.state != ExtensionState::Disconnected
        });
        if !source_is_current {
            self.discard_peer_activation_reservation(peer_context);
            return;
        }
        if let Some(reservation) = extension.activation_reservation
            && !self.reaccount_activation_reservation(source_id, reservation, event)
        {
            self.finish_pending_session_discovery_declaration(source_id);
            return;
        }

        match event {
            Event::ExtensionSessionContextProviderRegister(_) => {
                if self.should_stage_extension_capabilities(source_id) {
                    self.stage_session_context_provider_register(
                        source_id,
                        extension.admission.clone(),
                    );
                } else {
                    self.apply_session_context_provider_registration(source_id);
                }
            }
            Event::ExtensionSessionDiscoverySnapshotDeclared(snapshot) => {
                if self.should_stage_extension_capabilities(source_id) {
                    self.stage_session_discovery_snapshot(
                        source_id,
                        snapshot.clone(),
                        extension.admission.clone(),
                    );
                } else {
                    self.apply_session_discovery_snapshot(source_id, snapshot.clone());
                }
            }
            Event::ExtensionSessionContextReady(ready) => {
                if let Err(error) =
                    self.apply_extension_session_context_ready(source_id, ready.clone())
                {
                    self.publication.pending_error.get_or_insert(error);
                }
            }
            _ => unreachable!("caller filters session-discovery events"),
        }

        if extension.activation_reservation.is_some() {
            self.finish_pending_session_discovery_declaration(source_id);
        }
    }

    /// Apply one committed prompt-fragment declaration to the exact configured
    /// connection generation that authored it.
    pub(super) fn process_committed_prompt_fragment(
        &mut self,
        peer_context: &interception::PeerPublicationContext,
        event: &Event,
    ) {
        let Event::ExtPromptFragmentPublish(publish) = event else {
            unreachable!("caller filters prompt-fragment declarations");
        };
        let Some(extension) = peer_context.extension.as_ref() else {
            return;
        };
        let source_id = &extension.source;
        let source_is_current = self.extensions.entries.get(source_id).is_some_and(|entry| {
            entry.connection_id == extension.source
                && entry.instance_id == extension.instance_id
                && entry.name == extension.publisher
                && entry.kind == extension.kind
                && entry.state != ExtensionState::Disconnected
        });
        if !source_is_current {
            return;
        }
        if let Some(reservation) = extension.activation_reservation
            && !self.reaccount_activation_reservation(source_id, reservation, event)
        {
            self.finish_pending_prompt_fragment_declaration(source_id);
            return;
        }
        if self.should_stage_extension_capabilities(source_id) {
            self.stage_extension_prompt_fragment(source_id, publish.clone());
        } else {
            self.apply_extension_prompt_fragment(source_id, publish.clone());
        }
        if extension.activation_reservation.is_some() {
            self.finish_pending_prompt_fragment_declaration(source_id);
        }
    }

    /// Validate and apply one committed tool lifecycle declaration.
    pub(super) fn process_committed_tool_declaration(
        &mut self,
        peer_context: &interception::PeerPublicationContext,
        event: &Event,
    ) {
        let Some(extension) = peer_context
            .extension
            .as_ref()
            .filter(|extension| matches!(extension.kind, ClientKind::Tool | ClientKind::Core))
        else {
            return;
        };
        let source_id = &extension.source;
        let source_is_current = self.extensions.entries.get(source_id).is_some_and(|entry| {
            entry.connection_id == extension.source
                && entry.instance_id == extension.instance_id
                && entry.name == extension.publisher
                && matches!(entry.kind, ClientKind::Tool | ClientKind::Core)
                && entry.state != ExtensionState::Disconnected
        });
        if !source_is_current {
            // A committed declaration keeps its captured publisher envelope, but
            // an obsolete generation cannot mutate or release the replacement
            // generation's registry/staging state.
            return;
        }
        if let Some(reservation) = extension.activation_reservation
            && !self.reaccount_activation_reservation(source_id, reservation, event)
        {
            self.finish_pending_tool_lifecycle_declaration(source_id);
            return;
        }

        match event {
            Event::ToolRegistrationDeclared(registration) => {
                if self.validate_or_reject_assigned_prefix(source_id, registration) {
                    if self.should_stage_extension_capabilities(source_id)
                        && extension.activation_reservation.is_some()
                    {
                        self.stage_extension_tool_registration(source_id, registration.clone());
                    } else {
                        self.register_extension_tool(
                            source_id,
                            extension.publisher.clone(),
                            extension.instance_id,
                            registration.clone(),
                        );
                    }
                }
            }
            Event::ToolUnregistrationDeclared(unregister) => {
                self.handle_extension_tool_unregister(
                    source_id,
                    extension.publisher.clone(),
                    extension.instance_id,
                    unregister.clone(),
                );
            }
            _ => unreachable!("caller filters tool lifecycle declarations"),
        }

        if extension.activation_reservation.is_some() {
            self.finish_pending_tool_lifecycle_declaration(source_id);
        }
    }

    /// Resize a pre-activation frame reservation to its committed replacement.
    pub(super) fn reaccount_activation_reservation(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        reservation: interception::ActivationReservation,
        event: &Event,
    ) -> bool {
        let replacement_bytes = Self::encoded_emit_size(event, reservation.persist);
        let Some(stage) = self.extensions.activation_staging.get_mut(source_id) else {
            return false;
        };
        let next_bytes = stage
            .retained_message_bytes
            .saturating_sub(reservation.encoded_bytes)
            .saturating_add(replacement_bytes);
        if MAX_EXTENSION_ACTIVATION_BYTES < next_bytes {
            stage.retained_message_count = stage.retained_message_count.saturating_sub(1);
            stage.retained_message_bytes = stage
                .retained_message_bytes
                .saturating_sub(reservation.encoded_bytes);
            let message = format!(
                "extension activation staging exceeds {} messages or {} encoded bytes after interception",
                MAX_EXTENSION_ACTIVATION_MESSAGES, MAX_EXTENSION_ACTIVATION_BYTES
            );
            if let Err(error) = self.handle_extension_protocol_failure(source_id, message) {
                self.publication.pending_error.get_or_insert(error);
            }
            return false;
        }
        stage.retained_message_bytes = next_bytes;
        true
    }

    /// Release activation quota for a declaration dropped by interception.
    pub(super) fn discard_peer_activation_reservation(
        &mut self,
        peer_context: &interception::PeerPublicationContext,
    ) {
        let Some(extension) = peer_context.extension.as_ref() else {
            return;
        };
        let Some(reservation) = extension.activation_reservation else {
            return;
        };
        if !self
            .extensions
            .entries
            .get(&extension.source)
            .is_some_and(|entry| {
                entry.instance_id == extension.instance_id
                    && entry.state != ExtensionState::Disconnected
            })
        {
            return;
        }
        if let Some(stage) = self
            .extensions
            .activation_staging
            .get_mut(&extension.source)
        {
            stage.retained_message_count = stage.retained_message_count.saturating_sub(1);
            stage.retained_message_bytes = stage
                .retained_message_bytes
                .saturating_sub(reservation.encoded_bytes);
        }
        match reservation.declaration_family {
            interception::ActivationDeclarationFamily::ProviderModels => {
                self.finish_pending_provider_model_declaration(&extension.source);
            }
            interception::ActivationDeclarationFamily::ToolLifecycle => {
                self.finish_pending_tool_lifecycle_declaration(&extension.source);
            }
            interception::ActivationDeclarationFamily::ActionSchema => {
                self.finish_pending_action_schema_declaration(&extension.source);
            }
            interception::ActivationDeclarationFamily::PromptFragment => {
                self.finish_pending_prompt_fragment_declaration(&extension.source);
            }
            interception::ActivationDeclarationFamily::SessionDiscovery => {
                self.finish_pending_session_discovery_declaration(&extension.source);
            }
            interception::ActivationDeclarationFamily::AgentContext => {
                self.finish_pending_agent_context_declaration(&extension.source);
            }
        }
    }

    /// Release one admitted pre-`Ready` declaration and retry activation.
    pub(super) fn finish_pending_provider_model_declaration(
        &mut self,
        source_id: &tau_proto::ConnectionId,
    ) {
        self.finish_pending_activation_declaration(
            source_id,
            interception::ActivationDeclarationFamily::ProviderModels,
        );
    }

    /// Release one admitted pre-`Ready` tool declaration and retry activation.
    pub(super) fn finish_pending_tool_lifecycle_declaration(
        &mut self,
        source_id: &tau_proto::ConnectionId,
    ) {
        self.finish_pending_activation_declaration(
            source_id,
            interception::ActivationDeclarationFamily::ToolLifecycle,
        );
    }

    /// Release one pre-activation Action snapshot reservation.
    pub(super) fn finish_pending_action_schema_declaration(
        &mut self,
        source_id: &tau_proto::ConnectionId,
    ) {
        self.finish_pending_activation_declaration(
            source_id,
            interception::ActivationDeclarationFamily::ActionSchema,
        );
    }

    /// Release one admitted pre-`Ready` prompt-fragment declaration and retry
    /// activation.
    pub(super) fn finish_pending_prompt_fragment_declaration(
        &mut self,
        source_id: &tau_proto::ConnectionId,
    ) {
        self.finish_pending_activation_declaration(
            source_id,
            interception::ActivationDeclarationFamily::PromptFragment,
        );
    }

    /// Release one admitted pre-`Ready` session-discovery declaration and retry
    /// activation.
    pub(super) fn finish_pending_session_discovery_declaration(
        &mut self,
        source_id: &tau_proto::ConnectionId,
    ) {
        self.finish_pending_activation_declaration(
            source_id,
            interception::ActivationDeclarationFamily::SessionDiscovery,
        );
    }

    /// Release one admitted pre-`Ready` per-agent context declaration and retry
    /// activation.
    pub(super) fn finish_pending_agent_context_declaration(
        &mut self,
        source_id: &tau_proto::ConnectionId,
    ) {
        self.finish_pending_activation_declaration(
            source_id,
            interception::ActivationDeclarationFamily::AgentContext,
        );
    }

    /// Release one family-specific pending count and retry extension
    /// activation.
    pub(super) fn finish_pending_activation_declaration(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        declaration_family: interception::ActivationDeclarationFamily,
    ) {
        let source_id = source_id.clone();
        let pending = match declaration_family {
            interception::ActivationDeclarationFamily::ProviderModels => {
                &mut self.extensions.pending_provider_model_declarations
            }
            interception::ActivationDeclarationFamily::ToolLifecycle => {
                &mut self.extensions.pending_tool_lifecycle_declarations
            }
            interception::ActivationDeclarationFamily::ActionSchema => {
                &mut self.extensions.pending_action_schema_declarations
            }
            interception::ActivationDeclarationFamily::PromptFragment => {
                &mut self.extensions.pending_prompt_fragment_declarations
            }
            interception::ActivationDeclarationFamily::SessionDiscovery => {
                &mut self.extensions.pending_session_discovery_declarations
            }
            interception::ActivationDeclarationFamily::AgentContext => {
                &mut self.extensions.pending_agent_context_declarations
            }
        };
        let remove = if let Some(count) = pending.get_mut(&source_id) {
            *count = count.saturating_sub(1);
            *count == 0
        } else {
            false
        };
        if remove {
            pending.remove(&source_id);
        }
        if self.publication.pending_error.is_none()
            && let Err(error) = self.maybe_finish_extension_activation(Some(&source_id))
        {
            self.publication.pending_error = Some(error);
        }
    }

    /// Propagate a fatal error raised synchronously by downstream publish work.
    pub(super) fn take_pending_publish_error(&mut self) -> Result<(), HarnessError> {
        match self.publication.pending_error.take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Apply one committed Action declaration or terminal report.
    pub(super) fn process_committed_action_event(
        &mut self,
        peer_context: &interception::PeerPublicationContext,
        event: &Event,
    ) {
        let Some(extension) = peer_context.extension.as_ref().filter(|extension| {
            matches!(
                extension.kind,
                ClientKind::Provider | ClientKind::Tool | ClientKind::Action | ClientKind::Core
            ) && extension
                .capabilities
                .contains(&tau_proto::PeerCapability::ActionProvider)
        }) else {
            return;
        };
        let source_id = &extension.source;
        let source_is_current = self.extensions.entries.get(source_id).is_some_and(|entry| {
            entry.connection_id == extension.source
                && entry.instance_id == extension.instance_id
                && entry.name == extension.publisher
                && entry.kind == extension.kind
                && entry
                    .peer_capabilities
                    .contains(&tau_proto::PeerCapability::ActionProvider)
                && entry.state != ExtensionState::Disconnected
        });
        if !source_is_current {
            return;
        }
        match event {
            Event::ActionSchemaDeclared(declaration) => {
                if let Some(reservation) = extension.activation_reservation
                    && !self.reaccount_activation_reservation(source_id, reservation, event)
                {
                    self.finish_pending_action_schema_declaration(source_id);
                    return;
                }
                if self.should_stage_extension_capabilities(source_id)
                    && extension.activation_reservation.is_some()
                {
                    self.stage_action_schema(source_id, declaration.schema.clone());
                } else {
                    self.publish_action_schema(
                        source_id,
                        extension.publisher.clone(),
                        extension.instance_id,
                        declaration.schema.clone(),
                    );
                }
                if extension.activation_reservation.is_some() {
                    self.finish_pending_action_schema_declaration(source_id);
                }
            }
            Event::ActionResultReported(result) => {
                self.handle_action_result(extension, result.clone());
            }
            Event::ActionErrorReported(error) => {
                self.handle_action_error(extension, error.clone());
            }
            _ => unreachable!("caller filters committed Action peer events"),
        }
    }

    pub(super) fn handle_extension_tool_unregister(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        publisher_extension_id: ExtensionName,
        publisher_instance_id: tau_proto::ExtensionInstanceId,
        unregister: tau_proto::ToolUnregistrationDeclared,
    ) {
        let removed_staged = self.remove_staged_tool_registration(source_id, &unregister.tool_name);
        if self.should_stage_extension_capabilities(source_id) {
            if removed_staged {
                return;
            }
        } else {
            let visible_name = self
                .registry
                .providers_for(unregister.tool_name.as_str())
                .into_iter()
                .find(|provider| provider.connection_id == *source_id)
                .map(|provider| self.tool_model_visible_name(&provider.tool).clone())
                .unwrap_or_else(|| unregister.tool_name.clone());
            let removed = self
                .registry
                .unregister(source_id, unregister.tool_name.as_str());
            if removed {
                if self
                    .registry
                    .providers_for(unregister.tool_name.as_str())
                    .is_empty()
                {
                    self.mark_tool_unavailable_for_notice(
                        unregister.tool_name.clone(),
                        visible_name,
                    );
                }
                self.publish_event(
                    Some(crate::harness::harness_connection_id()),
                    Event::ToolUnregister(tau_proto::ToolUnregister {
                        publisher_extension_id,
                        publisher_instance_id,
                        tool_name: unregister.tool_name,
                    }),
                );
                return;
            }
        }
        self.emit_notice(
            tau_proto::notice_kind::HARNESS_INTERNAL_WARNING,
            tau_proto::NoticeLevel::Critical,
            tau_proto::NoticePurpose::Alert,
            &format!(
                "Rejected tool unregistration from `{source_id}`: `{}` is not owned by this extension",
                unregister.tool_name
            ),
        );
    }

    pub(super) fn reject_unroutable_extension_tool_request(
        &mut self,
        request: ToolRequest,
        tool_name: ToolName,
    ) {
        let message = unavailable_tool_error_message(&tool_name);
        self.reject_peer_tool_request(request, tool_name, message);
    }

    /// Publish ordered rejection and terminal closure for one committed peer
    /// request, then retain its completed-call tombstone.
    pub(super) fn reject_peer_tool_request(
        &mut self,
        request: ToolRequest,
        tool_name: ToolName,
        message: String,
    ) {
        let owning_cid = self.tool_runtime.tool_agents.get(&request.call_id).cloned();
        let rejected = ToolRejected {
            call_id: request.call_id.clone(),
            tool_name: tool_name.clone(),
            tool_type: request.tool_type,
            message: message.clone(),
            originator: request.originator.clone(),
        };
        let event = Event::ToolRejected(rejected);
        match owning_cid.as_ref() {
            Some(cid) => {
                self.publish_for_agent_from(
                    cid,
                    Some(crate::harness::harness_connection_id()),
                    event,
                );
            }
            None => self.publish_event(Some(crate::harness::harness_connection_id()), event),
        }
        let error = ToolError {
            presentation: Default::default(),
            call_id: request.call_id,
            tool_name: tool_name.clone(),
            tool_type: request.tool_type,
            message,
            details: None,
            originator: request.originator,

            display: None,
        };
        self.publish_terminal_tool_error(
            owning_cid.as_ref(),
            Some(crate::harness::harness_connection_id()),
            error,
        );
    }

    pub(super) fn handle_extension_tool_terminal_event(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        event: Event,
    ) -> Option<Event> {
        match event {
            Event::ToolResult(result) if source_id == harness_connection_id() => {
                self.handle_extension_tool_result(source_id, result);
                None
            }
            Event::ToolError(error) if source_id == harness_connection_id() => {
                self.handle_extension_tool_error(source_id, error);
                None
            }
            Event::ToolCancelled(cancelled) if source_id == harness_connection_id() => {
                self.handle_extension_tool_cancelled(source_id, cancelled);
                None
            }
            // Keep this peer-authored event rejection in sync with
            // `is_peer_forbidden_harness_fact` and the immutable/must-pass
            // classifications in `harness/interception.rs`.
            Event::ToolResult(_)
            | Event::ToolResultDisplay(_)
            | Event::ToolError(_)
            | Event::ToolCancelled(_)
            | Event::ProviderToolResult(_)
            | Event::ProviderToolError(_)
            | Event::SessionStarted(_)
            | Event::SessionShutdown(_)
            | Event::SessionAgentLoaded(_)
            | Event::SessionAgentUnloaded(_)
            | Event::AgentStarted(_)
            | Event::AgentMessageSent(_)
            | Event::AgentMessageReceived(_) => None,
            Event::ToolBackgroundResult(_)
            | Event::ToolBackgroundResultDisplay(_)
            | Event::ToolBackgroundError(_)
                if source_id != harness_connection_id() =>
            {
                None
            }
            other => Some(other),
        }
    }

    pub(super) fn handle_extension_tool_result(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        mut result: ToolResult,
    ) {
        if !self.validate_tool_event_source(&result.call_id, source_id) {
            return;
        }
        if self.tool_runtime.tool_turn.is_backgrounded(&result.call_id) {
            self.handle_background_tool_result(crate::harness::harness_connection_id(), result);
        } else if let Some(cid) = self.tool_runtime.tool_agents.get(&result.call_id).cloned() {
            let mut allows_provider_image = false;
            if let Some(tool) = self.tool_runtime.pending_tools.get(&result.call_id) {
                tool.restore_terminal_result_metadata(&mut result);
                allows_provider_image = tool.allows_provider_image;
            }
            let has_provider_image = !result.provider_content.is_empty();
            let validation = if has_provider_image && !allows_provider_image {
                Err("the originating tool is not authorized for image output".to_owned())
            } else {
                self.agent_registry
                    .agents
                    .get(&cid)
                    .and_then(|agent| {
                        let agent_id = agent.agent_id.clone()?;
                        let parent = agent.head.map_or(
                            tau_core::AgentEventParent::Root,
                            tau_core::AgentEventParent::Under,
                        );
                        Some((agent_id, parent))
                    })
                    .ok_or_else(|| "the owning agent is unavailable".to_owned())
                    .and_then(|(agent_id, parent)| {
                        self.agent_store
                            .validate_agent_event_at(
                                &agent_id,
                                Some(tau_core::PersistedEventSource::Connection(
                                    crate::harness::harness_connection_id().clone(),
                                )),
                                parent,
                                &Event::ProviderToolResult(result.clone()),
                                tau_proto::UnixMicros::now(),
                            )
                            .map_err(|error| error.to_string())
                    })
            };
            if let Err(error) = validation {
                tracing::warn!(
                    target: "tau_harness",
                    call_id = %result.call_id,
                    %error,
                    "rejecting tool result before dedup and generic publication"
                );
                self.publish_terminal_tool_error(
                    Some(&cid),
                    Some(crate::harness::harness_connection_id()),
                    ToolError {
                        presentation: Default::default(),
                        call_id: result.call_id,
                        tool_name: result.tool_name,
                        tool_type: result.tool_type,
                        message: if has_provider_image {
                            "image result rejected by media safety validation".to_owned()
                        } else {
                            "tool result rejected by transcript safety validation".to_owned()
                        },
                        details: None,
                        display: None,
                        originator: result.originator,
                    },
                );
                return;
            }
            // Collapse byte-identical large results into a pointer back to the
            // first call_id that produced this content on this agent's branch.
            // See `crate::dedup` for the design.
            self.dedup_tool_result(&cid, &mut result);
            // Snap to the owning agent's head before folding the result. Without
            // this, a sibling side conv that just touched the parent agent
            // (during its teardown) leaves `tree.head` on the *parent* branch —
            // folding the result there misplaces it and produces orphan ToolUse
            // blocks when the parent conv is later re-prompted.
            self.publish_terminal_tool_result(
                Some(&cid),
                Some(crate::harness::harness_connection_id()),
                result,
            );
        } else if self
            .tool_runtime
            .peer_tool_requests
            .contains(&result.call_id)
            && let Some(tool) = self
                .tool_runtime
                .pending_tools
                .get(&result.call_id)
                .cloned()
        {
            result.tool_name = tool.name;
            result.tool_type = tool.tool_type;
            if !result.provider_content.is_empty() {
                self.publish_terminal_tool_error(
                    None,
                    Some(crate::harness::harness_connection_id()),
                    ToolError {
                        presentation: Default::default(),
                        call_id: result.call_id,
                        tool_name: result.tool_name,
                        tool_type: result.tool_type,
                        message: "image result rejected for ownerless peer tool request".to_owned(),
                        details: None,
                        display: None,
                        originator: result.originator,
                    },
                );
            } else {
                self.publish_terminal_tool_result(
                    None,
                    Some(crate::harness::harness_connection_id()),
                    result,
                );
            }
        } else {
            self.emit_info(&format!(
                "discarding duplicate tool result for call_id={}",
                result.call_id
            ));
        }
    }

    pub(super) fn handle_extension_tool_error(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        mut error: ToolError,
    ) {
        if !self.validate_tool_event_source(&error.call_id, source_id) {
            return;
        }
        if self.tool_runtime.tool_turn.is_backgrounded(&error.call_id) {
            self.handle_background_tool_error(Some(crate::harness::harness_connection_id()), error);
        } else if let Some(cid) = self.tool_runtime.tool_agents.get(&error.call_id).cloned() {
            if let Some(tool) = self.tool_runtime.pending_tools.get(&error.call_id) {
                error.tool_name = tool.name.clone();
                error.tool_type = tool.tool_type;
            }
            self.dedup_tool_error(&cid, &mut error);
            self.publish_terminal_tool_error(
                Some(&cid),
                Some(crate::harness::harness_connection_id()),
                error,
            );
        } else if self
            .tool_runtime
            .peer_tool_requests
            .contains(&error.call_id)
            && let Some(tool) = self.tool_runtime.pending_tools.get(&error.call_id).cloned()
        {
            error.tool_name = tool.name;
            error.tool_type = tool.tool_type;
            self.publish_terminal_tool_error(
                None,
                Some(crate::harness::harness_connection_id()),
                error,
            );
        } else {
            self.emit_info(&format!(
                "discarding duplicate tool error for call_id={}",
                error.call_id
            ));
        }
    }

    pub(super) fn handle_extension_tool_cancelled(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        mut cancelled: ToolCancelled,
    ) {
        if !self.validate_tool_event_source(&cancelled.call_id, source_id) {
            return;
        }
        if self
            .tool_runtime
            .tool_turn
            .is_backgrounded(&cancelled.call_id)
        {
            self.handle_background_tool_cancelled(
                crate::harness::harness_connection_id(),
                cancelled,
            );
        } else if let Some(cid) = self
            .tool_runtime
            .tool_agents
            .get(&cancelled.call_id)
            .cloned()
        {
            let call_id = cancelled.call_id.clone();
            if let Some(tool) = self.tool_runtime.pending_tools.get(&cancelled.call_id) {
                cancelled.tool_name = tool.name.clone();
                cancelled.tool_type = tool.tool_type;
            }
            if self.tool_terminal_has_open_durable_owner(&cid, &call_id) {
                let cause = self
                    .tool_runtime
                    .pending_cancellation_observations
                    .get(&call_id)
                    .copied()
                    .map_or(tau_proto::ToolTerminalCause::Unknown, |request| {
                        tau_proto::ToolTerminalCause::Cancellation { request }
                    });
                self.observe_tool_terminal(&cid, &call_id, cause);
                self.publish_for_agent_from(
                    &cid,
                    Some(crate::harness::harness_connection_id()),
                    Event::ToolCancelled(cancelled),
                );
            } else {
                self.publish_event(
                    Some(crate::harness::harness_connection_id()),
                    Event::ToolCancelled(cancelled),
                );
                self.record_wait_tool_cancelled(&HashSet::from([call_id.clone()]), None);
                self.on_tool_call_complete(call_id.as_str());
                self.clear_tool_call_tracking(call_id.as_str());
            }
        } else if self
            .tool_runtime
            .peer_tool_requests
            .contains(&cancelled.call_id)
            && let Some(tool) = self
                .tool_runtime
                .pending_tools
                .get(&cancelled.call_id)
                .cloned()
        {
            cancelled.tool_name = tool.name;
            cancelled.tool_type = tool.tool_type;
            self.publish_event(
                Some(crate::harness::harness_connection_id()),
                Event::ToolCancelled(cancelled),
            );
        }
    }

    /// Validate one committed shell report against its captured extension
    /// generation before consulting mutable routed-command state.
    pub(super) fn process_committed_shell_command_report(
        &mut self,
        peer_context: &interception::PeerPublicationContext,
        event: &Event,
    ) {
        let Some(extension) = peer_context
            .extension
            .as_ref()
            .filter(|extension| matches!(extension.kind, ClientKind::Tool | ClientKind::Core))
        else {
            return;
        };
        let source_is_current =
            self.extensions
                .entries
                .get(&extension.source)
                .is_some_and(|entry| {
                    entry.connection_id == extension.source
                        && extension.admission.session_id == self.current_session_id
                        && extension.admission.session_generation == self.current_session_generation
                        && entry.instance_id == extension.instance_id
                        && entry.name == extension.publisher
                        && entry.kind == extension.kind
                        && entry.state != ExtensionState::Disconnected
                });
        if !source_is_current {
            return;
        }
        self.canonicalize_committed_shell_command_report(&extension.source, event.clone());
    }

    /// Validate routed-command ownership and publish one canonical shell fact.
    pub(super) fn canonicalize_committed_shell_command_report(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        event: Event,
    ) {
        match event {
            Event::ShellCommandProgressReported(progress) => {
                let mut progress = progress;
                let route_id = UiShellRouteId::new(progress.command_id.clone());
                let Some(pending) = self.ui_runtime.pending_ui_shell_commands.get(&route_id) else {
                    tracing::warn!(
                        target: "tau_harness",
                        command_id = %progress.command_id,
                        source_id = %source_id,
                        "discarding stale or unknown shell command progress"
                    );
                    return;
                };
                if &pending.provider_id != source_id
                    || progress.target_agent_id != pending.command.target_agent_id
                {
                    tracing::warn!(
                        target: "tau_harness",
                        command_id = %progress.command_id,
                        source_id = %source_id,
                        expected_provider = %pending.provider_id,
                        "discarding shell command progress with invalid ownership or identity"
                    );
                    return;
                }
                progress.command_id = pending.command.command_id.clone();
                progress.target_agent_id = pending.command.target_agent_id.clone();
                if pending.targets_ephemeral {
                    self.mark_pending_ephemeral_shell_canonical(progress.command_id.clone());
                }
                self.publish_event(
                    Some(crate::harness::harness_connection_id()),
                    Event::ShellCommandProgress(progress),
                );
            }
            Event::ShellCommandFinishedReported(finished) => {
                let mut finished = finished;
                let route_id = UiShellRouteId::new(finished.command_id.clone());
                let Some(pending) = self
                    .ui_runtime
                    .pending_ui_shell_commands
                    .get(&route_id)
                    .cloned()
                else {
                    tracing::warn!(
                        target: "tau_harness",
                        command_id = %finished.command_id,
                        source_id = %source_id,
                        "discarding stale or duplicate shell command completion"
                    );
                    return;
                };
                let command = &pending.command;
                if &pending.provider_id != source_id
                    || finished.session_id != command.session_id
                    || finished.command != command.command
                    || finished.include_in_context != command.include_in_context
                    || finished.target_agent_id != command.target_agent_id
                {
                    tracing::warn!(
                        target: "tau_harness",
                        command_id = %finished.command_id,
                        source_id = %source_id,
                        expected_provider = %pending.provider_id,
                        "discarding shell command completion with invalid ownership or identity"
                    );
                    return;
                }
                self.ui_runtime.pending_ui_shell_commands.remove(&route_id);
                finished.command_id = command.command_id.clone();
                finished.session_id = command.session_id.clone();
                finished.command.clone_from(&command.command);
                finished.include_in_context = command.include_in_context;
                finished.target_agent_id = command.target_agent_id.clone();
                if pending.targets_ephemeral {
                    self.mark_pending_ephemeral_shell_canonical(finished.command_id.clone());
                }
                if finished.include_in_context {
                    self.ui_runtime
                        .pending_ui_shell_output_injections
                        .insert(finished.command_id.clone());
                }
                // The canonical completion commits before any transcript
                // injection, so the UI always finalizes its render block first.
                self.publish_event(
                    Some(crate::harness::harness_connection_id()),
                    Event::ShellCommandFinished(finished),
                );
            }
            _ => unreachable!("caller filters shell command reports"),
        }
    }

    /// Publish one fallback event while preserving its original frame-admission
    /// session across activation staging.
    pub(super) fn handle_extension_fallback_event_with_admission(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        event: Event,
        persist_override: Option<bool>,
        admission: ExtensionFrameAdmission,
    ) {
        self.handle_extension_fallback_event_with_optional_admission(
            source_id,
            event,
            persist_override,
            Some(admission),
        );
    }

    /// Shared fallback publication implementation with optional captured
    /// frame-admission metadata.
    pub(super) fn handle_extension_fallback_event_with_optional_admission(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        event: Event,
        persist_override: Option<bool>,
        admission: Option<ExtensionFrameAdmission>,
    ) {
        if !Self::is_extension_fallback_emit_allowed(&event) {
            return;
        }
        let persist = persist_override.unwrap_or_else(|| event.defaults_to_persist());
        if self.should_stage_extension_capabilities(source_id) {
            self.stage_extension_publish(source_id, event, persist);
        } else if let Some(admission) = admission {
            self.enqueue_publish_with_admission(
                Some(source_id),
                event,
                persist,
                false,
                None,
                admission,
            );
        } else {
            self.enqueue_publish(Some(source_id), event, persist, false, None);
        }
    }

    /// Convert one authorized routine extension notice request into a separate
    /// harness-authored live publication.
    pub(super) fn handle_extension_notice_request(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        request: tau_proto::ExtensionNoticeRequest,
    ) {
        let authorized = self
            .extensions
            .entries
            .get(source_id)
            .is_some_and(|entry| entry.state != ExtensionState::Disconnected);
        if !authorized {
            return;
        }
        let level = if request.level == tau_proto::NoticeLevel::Critical {
            tau_proto::NoticeLevel::Warning
        } else {
            request.level
        };
        self.enqueue_publish(
            Some(crate::harness::harness_connection_id()),
            Event::HarnessNotice(tau_proto::HarnessNotice::diagnostic(
                tau_proto::notice_kind::EXTENSION_NOTICE,
                request.message,
                level,
            )),
            false,
            false,
            None,
        );
    }
}
