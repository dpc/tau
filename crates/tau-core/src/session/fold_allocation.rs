//! Requested allocation accounting for the existing exclusive AgentTree fold.
//!
//! This is not retained-tree sizing or an allocator/RSS limit. Collection
//! estimates follow the current Rust Vec/hashbrown/BTree implementations.
//! Saturation fails admission closed. Payload measurement may invoke opaque
//! serializers: the caller must hold its separate validation-scratch permit.

use super::*;
use crate::semantic_persistence::fold_memory::{hash_growth, owned_value, vec_growth};

impl AgentTree {
    /// Bounds allocations caused by this event, without copying historical
    /// state.
    ///
    /// Count backing growth only for containers this fold can touch. Old
    /// payloads enter the charge only when the fold actually clones them
    /// (closing tool aggregate and dormant output-length repair).
    pub(crate) fn fold_allocation_charge(&self, record: &PersistedAgentEvent) -> usize {
        let event = &record.event;
        let mut charge = self
            .indexes
            .compaction_chains
            .observe_allocation_charge(event);
        let mut additions = NodeAdditions::default();
        let parent = record.parent.resolve(self.head);

        if event.name().category() == &tau_proto::EventCategory::Message {
            charge = charge.saturating_add(message_projection_charge(event));
            charge = charge.saturating_add(self.context_allocation_charge(
                self.head,
                false,
                &mut additions,
            ));
            return charge.saturating_add(self.node_allocation_charge(additions));
        }

        if record.parent != AgentEventParent::InheritHead
            && !self.is_ancestor_head(
                parent.map_or(AgentHead::Root, AgentHead::Node),
                self.head.map_or(AgentHead::Root, AgentHead::Node),
            )
        {
            // output_length_append_preserves_selection eagerly evaluates the
            // repair query even for events that cannot match its returned
            // variant.
            charge = charge.saturating_add(self.dormant_repair_allocation_charge());
            if let Event::ProviderResponseFinished(response) = event {
                charge = charge.saturating_add(
                    self.lineage_owner_allocation_charge(&response.agent_prompt_id),
                );
            }
        }
        charge = charge.saturating_add(self.control_allocation_charge(event));
        match event {
            Event::AgentStarted(started) => {
                charge = charge.saturating_add(owned_value(started));
                charge = charge
                    .saturating_add(metadata_growth(self.metadata.len(), started.metadata.len()));
            }
            Event::AgentDisplayNameSet(name) => {
                charge = charge.saturating_add(owned_value(&name.display_name));
            }
            Event::AgentInitializationContextSet(context) => {
                charge = charge.saturating_add(owned_value(context));
            }
            Event::AgentMetadataSet(set) => {
                charge = charge.saturating_add(owned_value(set));
                if !self.metadata.contains_key(&set.key) {
                    charge = charge.saturating_add(metadata_growth(self.metadata.len(), 1));
                }
            }
            Event::AgentOuterTurnStarted(started) => {
                charge = charge
                    .saturating_add(map_growth(&self.outer_turns, 1))
                    .saturating_add(owned_value(started))
                    .saturating_add(owned_value(&started.outer_turn_id));
            }
            Event::ToolBackgroundResult(result) => {
                charge = charge
                    .saturating_add(set_growth(&self.background_completed_tool_calls, 1))
                    .saturating_add(owned_value(&result.call_id));
            }
            Event::ToolBackgroundError(error) => {
                charge = charge
                    .saturating_add(set_growth(&self.background_completed_tool_calls, 1))
                    .saturating_add(owned_value(&error.call_id));
            }
            Event::AgentPassiveContextAppended(appended) => {
                charge = charge
                    .saturating_add(map_growth(&self.indexes.passive_context_operations, 1))
                    .saturating_add(owned_value(appended))
                    .saturating_add(prompt_parts_charge(&appended.text, 0))
                    .saturating_add(self.context_allocation_charge(parent, false, &mut additions));
            }
            Event::AgentPromptSubmitted(prompt) => {
                charge = charge
                    .saturating_add(owned_value(prompt))
                    .saturating_add(prompt_parts_charge(
                        &prompt.text,
                        prompt.trusted_internal_spans.len(),
                    ))
                    .saturating_add(self.context_allocation_charge(
                        parent,
                        prompt.internal_kind
                            == Some(tau_proto::InternalPromptKind::BackgroundToolCompletion),
                        &mut additions,
                    ));
            }
            Event::AgentPromptSteered(steered) => {
                charge = charge
                    .saturating_add(owned_value(steered))
                    .saturating_add(prompt_parts_charge(
                        &steered.text,
                        steered.trusted_internal_spans.len(),
                    ))
                    .saturating_add(self.context_allocation_charge(
                        parent,
                        steered.internal_kind
                            == Some(tau_proto::InternalPromptKind::BackgroundToolCompletion),
                        &mut additions,
                    ));
                // A deferred input has no node and creates no delivery yet;
                // charging this possible insertion remains conservative.
                if let Some(terminal) = &steered.self_compaction_terminal {
                    charge = charge
                        .saturating_add(map_growth(&self.self_compaction_deliveries, 1))
                        .saturating_add(owned_value(terminal))
                        .saturating_add(owned_value(&terminal.request_id));
                }
                if steered.internal_kind
                    == Some(tau_proto::InternalPromptKind::OutputLengthContinuation)
                {
                    // active_output_length_plan returns borrowed state; only
                    // its source prompt identifier is
                    // copied before folding.
                    if let Some((prompt_id, _)) = self.active_output_length_plan() {
                        charge = charge.saturating_add(owned_value(prompt_id));
                    }
                }
            }
            Event::AgentUserMessageInjected(injected) => {
                charge = charge
                    .saturating_add(owned_value(injected))
                    .saturating_add(prompt_parts_charge(&injected.text, 0))
                    .saturating_add(self.context_allocation_charge(parent, false, &mut additions));
            }
            Event::AgentMessageSent(message) => {
                charge = charge
                    .saturating_add(owned_value(message))
                    .saturating_add(self.context_allocation_charge(parent, false, &mut additions));
            }
            Event::AgentMessageReceived(message) => {
                charge = charge
                    .saturating_add(owned_value(message))
                    .saturating_add(self.context_allocation_charge(parent, false, &mut additions));
                if let Some(tau_proto::PeerMessageReview::Released { source }) =
                    &message.peer_review
                {
                    charge = charge
                        .saturating_add(owned_value(source))
                        .saturating_add(set_growth(&self.indexes.released_peer_reviews, 1));
                }
            }
            Event::AgentCompactionTriggered(_) => additions.nodes = 1,
            Event::AgentCompacted(compacted) => {
                additions.nodes = 1;
                additions.boundaries = 1;
                additions.models = 1;
                // The control fold already charged its full outcome copy. This
                // second copy bounds the replacement node plus model-index
                // value.
                charge = charge.saturating_add(owned_value(compacted));
            }
            Event::ProviderResponseFinished(response) => {
                let rejected = response.standalone_rejection_reason().is_some()
                    && self
                        .prompt_starts
                        .get(&response.agent_prompt_id)
                        .is_some_and(|started| {
                            started.operation == tau_proto::PromptOperation::StandaloneCompaction
                        });
                if !rejected {
                    additions.nodes = 1;
                    additions.responses = 1;
                    charge = charge.saturating_add(owned_value(response));
                    if let Some(started) = self.prompt_starts.get(&response.agent_prompt_id) {
                        additions.models = 1;
                        charge = charge.saturating_add(owned_value(&started.model));
                    }
                    let mut calls = 0usize;
                    for item in &response.output_items {
                        if let ContextItem::ToolCall(call) = item {
                            calls = calls.saturating_add(1);
                            // seen, call_order, and reverse call-to-round
                            // index.
                            charge =
                                charge.saturating_add(owned_value(&call.call_id).saturating_mul(3));
                        }
                    }
                    charge = charge
                        .saturating_add(new_vec::<ToolCallId>(calls))
                        .saturating_add(hash_growth::<ToolCallId>(0, 0, calls));
                    let owned = self
                        .pending_inference_inputs
                        .iter()
                        .filter(|input| input.owner_prompt_id == response.agent_prompt_id)
                        .count();
                    charge = charge.saturating_add(self.partition_allocation_charge(owned));
                    if calls == 0 {
                        additions.add_inference_inputs(self, &response.agent_prompt_id);
                        // materialize_after performs a second stable sort.
                        charge =
                            charge.saturating_add(sort_scratch::<PendingInferenceInput>(owned));
                    } else {
                        charge = charge
                            .saturating_add(map_growth(&self.pending_tool_rounds, 1))
                            .saturating_add(map_growth(&self.tool_call_rounds, calls))
                            .saturating_add(vec_growth(&self.pending_context_inputs, owned));
                    }
                }
            }
            Event::AgentPromptTerminated(terminated) => {
                let owned = self
                    .pending_inference_inputs
                    .iter()
                    .filter(|input| input.owner_prompt_id == terminated.agent_prompt_id)
                    .count();
                charge = charge
                    .saturating_add(self.partition_allocation_charge(owned))
                    .saturating_add(hash_growth::<(PersistedAgentEventSeq, NodeId)>(0, 0, owned));
                additions.add_inference_inputs(self, &terminated.agent_prompt_id);
            }
            Event::ProviderToolResult(result) => {
                let item = owned_value(&result.call_id)
                    .saturating_add(owned_value(&result.provider_content))
                    .saturating_add(tool_response_allocation_charge(&result.result));
                charge = charge.saturating_add(self.tool_result_allocation_charge(
                    &result.call_id,
                    item,
                    &mut additions,
                ));
            }
            Event::ProviderToolError(error) => {
                let item = owned_value(&error.call_id)
                    .saturating_add(owned_value(&error.message))
                    .saturating_add(tool_response_allocation_charge(
                        error
                            .details
                            .as_ref()
                            .unwrap_or(&tau_proto::CborValue::Null),
                    ));
                charge = charge.saturating_add(self.tool_result_allocation_charge(
                    &error.call_id,
                    item,
                    &mut additions,
                ));
            }
            Event::ToolCancelled(cancelled) => {
                let item = owned_value(&cancelled.call_id)
                    .saturating_add("cancelled".len())
                    .saturating_add(tool_response_allocation_charge(&tau_proto::CborValue::Null));
                charge = charge.saturating_add(self.tool_result_allocation_charge(
                    &cancelled.call_id,
                    item,
                    &mut additions,
                ));
            }
            _ => {}
        }
        charge.saturating_add(self.node_allocation_charge(additions))
    }

    /// Bounds tree-dependent validation scratch before any opaque
    /// serialization.
    ///
    /// This API has no candidate parent, so node counts bound any validated
    /// branch/window. It never charges historical message or response payloads.
    /// Event-local parsing, output buffers, and error text belong to the
    /// caller's separate validation scratch; codec-internal allocations
    /// remain separate.
    pub(crate) fn validation_allocation_charge(&self, event: &Event) -> usize {
        match event {
            Event::AgentOuterTurnStarted(_) => new_vec::<NodeId>(self.nodes.len()),
            Event::AgentStandaloneCompactionStarted(started)
                if matches!(
                    started.trigger,
                    tau_proto::StandaloneCompactionTrigger::AutomaticThresholdEvidence { .. }
                        | tau_proto::StandaloneCompactionTrigger::ManualAgentTool { .. }
                ) =>
            {
                // Active-window transcript materialization, plus the proactive
                // evidence node set. Nodes are borrowed; payloads are not
                // cloned.
                new_vec::<(NodeId, &AgentEntry)>(self.nodes.len()).saturating_add(hash_growth::<
                    NodeId,
                >(
                    0,
                    0,
                    self.nodes.len(),
                ))
            }
            Event::AgentStandaloneCompactionFailed(failed)
                if failed.context_retreat.is_some()
                    || matches!(
                        failed.reason,
                        tau_proto::StandaloneCompactionFailureReason::LocalSummaryUnusable
                            | tau_proto::StandaloneCompactionFailureReason::ContextIrreducible
                    ) =>
            {
                new_vec::<(NodeId, &AgentEntry)>(self.nodes.len())
            }
            Event::AgentPromptTerminated(terminated)
                if terminated
                    .automatic_compaction_decision
                    .as_ref()
                    .is_some_and(|decision| decision.evidence.is_some()) =>
            {
                new_vec::<(NodeId, &AgentEntry)>(self.nodes.len()).saturating_add(hash_growth::<
                    NodeId,
                >(
                    0,
                    0,
                    self.nodes.len(),
                ))
            }
            Event::ProviderResponseFinished(response) => self
                .lineage_owner_allocation_charge(&response.agent_prompt_id)
                .saturating_add(self.active_outer_turn.as_ref().map_or(0, owned_value)),
            Event::ToolBackgroundResult(_) | Event::ToolBackgroundError(_) => {
                let mut calls = 0usize;
                let mut charge = new_vec::<NodeId>(self.nodes.len());
                for node in &self.nodes {
                    if let AgentEntry::AssistantResponse { output_items, .. } = &node.entry {
                        for item in output_items {
                            if let ContextItem::ToolCall(call) = item {
                                calls = calls.saturating_add(1);
                                // Call IDs are plain string wrappers, not
                                // opaque
                                // ContextItems and cannot invoke raw-JSON
                                // parsing.
                                charge = charge.saturating_add(owned_value(&call.call_id));
                            }
                        }
                    }
                }
                charge.saturating_add(hash_growth::<ToolCallId>(0, 0, calls))
            }
            _ => 0,
        }
    }

    /// Charges only indexes receiving newly materialized nodes.
    fn node_allocation_charge(&self, additions: NodeAdditions) -> usize {
        vec_growth(&self.nodes, additions.nodes)
            .saturating_add(vec_growth(
                &self.indexes.provider_window.positions,
                additions.nodes,
            ))
            .saturating_add(map_growth(
                &self.indexes.provider_window.boundary_anchors,
                additions.boundaries,
            ))
            .saturating_add(map_growth(
                &self.indexes.provider_models_by_node,
                additions.models,
            ))
            .saturating_add(map_growth(
                &self.indexes.context_nodes_by_event_seq,
                additions.context,
            ))
            .saturating_add(map_growth(
                &self.indexes.assistant_response_nodes_by_event_seq,
                additions.responses,
            ))
            .saturating_add(set_growth(
                &self.indexes.background_completion_prompt_nodes,
                additions.background,
            ))
    }

    /// Charges the chosen immediate/deferred input destination without cloning
    /// it.
    fn context_allocation_charge(
        &self,
        parent: Option<NodeId>,
        background: bool,
        additions: &mut NodeAdditions,
    ) -> usize {
        if self.open_tool_round_applies_to(parent) {
            return vec_growth(&self.pending_context_inputs, 1)
                .saturating_add(size_of::<AgentEntry>());
        }
        // Mirror marked_inference_owner_for using borrowed IDs: that method
        // clones its result and therefore must not run before its charge
        // exists.
        let mut owners = self.inference_dispatches.values().filter(|dispatch| {
            parent.is_some_and(|parent| {
                !dispatch.finished
                    && dispatch.fold_semantics
                        == AgentJournalFoldSemantics::InferenceDeferredInputV1
                    && dispatch.head_move_generation == self.head_move_generation
                    && self.is_ancestor_head(dispatch.checkpoint.through, AgentHead::Node(parent))
            })
        });
        if let Some(owner) = owners.next()
            && owners.next().is_none()
        {
            return vec_growth(&self.pending_inference_inputs, 1)
                .saturating_add(size_of::<AgentEntry>())
                .saturating_add(owned_value(&owner.checkpoint.agent_prompt_id));
        }
        additions.nodes = additions.nodes.saturating_add(1);
        additions.context = additions.context.saturating_add(1);
        additions.background = additions.background.saturating_add(usize::from(background));
        0
    }

    /// Bounds both partition destinations and stable-sort scratch, not
    /// payloads.
    fn partition_allocation_charge(&self, owned: usize) -> usize {
        new_vec::<PendingInferenceInput>(owned)
            .saturating_add(new_vec::<PendingInferenceInput>(
                self.pending_inference_inputs.len().saturating_sub(owned),
            ))
            .saturating_add(sort_scratch::<PendingInferenceInput>(owned))
    }

    /// Includes old retained tool results only when this result closes the
    /// round.
    fn tool_result_allocation_charge(
        &self,
        call_id: &ToolCallId,
        item_charge: usize,
        additions: &mut NodeAdditions,
    ) -> usize {
        let Some(round) = self
            .tool_call_rounds
            .get(call_id)
            .and_then(|node| self.pending_tool_rounds.get(node))
        else {
            return item_charge;
        };
        let mut charge = item_charge
            .saturating_add(owned_value(call_id))
            .saturating_add(map_growth(&round.terminal_results, 1));
        if round.terminal_results.len().saturating_add(1) != round.call_order.len() {
            return charge;
        }
        charge = charge
            .saturating_add(item_charge)
            .saturating_add(new_vec::<ToolResultItem>(round.call_order.len()))
            .saturating_add(map_growth(
                &self.terminal_tool_result_nodes,
                round.call_order.len(),
            ))
            .saturating_add(sort_scratch::<PendingToolContextInput>(
                self.pending_context_inputs.len(),
            ));
        for result in round.terminal_results.values() {
            charge = charge.saturating_add(owned_value(result));
        }
        for id in &round.call_order {
            charge = charge.saturating_add(owned_value(id));
        }
        additions.nodes = additions
            .nodes
            .saturating_add(1)
            .saturating_add(self.pending_context_inputs.len());
        additions.context = additions
            .context
            .saturating_add(self.pending_context_inputs.len());
        additions.background = additions.background.saturating_add(
            self.pending_context_inputs
                .iter()
                .filter(|input| input.background_completion_prompt)
                .count(),
        );
        charge
    }
}

/// Maximum insertions into the node indexes during this one fold.
#[derive(Default)]
struct NodeAdditions {
    /// All newly materialized transcript entries.
    nodes: usize,
    /// Input occurrences indexed by durable sequence.
    context: usize,
    /// Assistant response occurrences.
    responses: usize,
    /// Nodes with a producing model.
    models: usize,
    /// Compaction replacement boundary anchors.
    boundaries: usize,
    /// Input nodes with background-completion provenance.
    background: usize,
}

impl NodeAdditions {
    /// Counts moved inference inputs; their old boxed payloads are not cloned.
    fn add_inference_inputs(&mut self, tree: &AgentTree, owner: &tau_proto::AgentPromptId) {
        for input in &tree.pending_inference_inputs {
            if &input.owner_prompt_id == owner {
                self.nodes = self.nodes.saturating_add(1);
                self.context = self.context.saturating_add(1);
                self.background = self
                    .background
                    .saturating_add(usize::from(input.background_completion_prompt));
            }
        }
    }
}

/// Uses concrete key/value layout without requiring serializable private folds.
fn map_growth<K, V>(map: &HashMap<K, V>, additional: usize) -> usize {
    hash_growth::<(K, V)>(map.len(), map.capacity(), additional)
}

/// Uses the set's concrete bucket layout.
fn set_growth<T>(set: &HashSet<T>, additional: usize) -> usize {
    hash_growth::<T>(set.len(), set.capacity(), additional)
}

/// Bounds allocations of a fresh vector, including sequential push growth.
fn new_vec<T>(len: usize) -> usize {
    vec_growth(&Vec::<T>::new(), len)
}

/// Current driftsort requests max(input length, SMALL_SORT_GENERAL_SCRATCH_LEN)
/// slots at most; the current general small-sort scratch minimum is 48.
fn sort_scratch<T>(len: usize) -> usize {
    if len < 2 {
        0
    } else {
        len.max(48).saturating_mul(size_of::<T>())
    }
}

/// Bounds root-to-leaf splits in the current 11-slot Rust BTree representation.
fn metadata_growth(len: usize, additional: usize) -> usize {
    if additional == 0 {
        return 0;
    }
    let entries = len.saturating_add(additional);
    // Every internal level has at least two descendants; binary height is
    // deliberately weaker than BTree's actual minimum branching factor.
    let height = (usize::BITS - entries.leading_zeros()) as usize;
    let node = size_of::<(tau_proto::AgentMetadataKey, AgentMetadataEntry)>()
        .saturating_mul(11)
        // Up to 12 child pointers, parent pointer/index, length and alignment.
        .saturating_add(size_of::<usize>().saturating_mul(16));
    additional
        .saturating_mul(height.saturating_add(2))
        .saturating_mul(node)
}

/// Accounts for cloned source text, split substrings and the two new vectors.
fn prompt_parts_charge(text: &str, spans: usize) -> usize {
    let parts = spans.saturating_mul(2).saturating_add(1);
    new_vec::<ContextItem>(1)
        .saturating_add(new_vec::<ContentPart>(parts))
        // The caller already charged the source clone; trusted-span splitting
        // retains that source while making disjoint newly owned substrings.
        .saturating_add(if spans == 0 { 0 } else { text.len() })
}

impl AgentTree {
    /// Bounds the optional owner clone without calling the allocating query.
    fn lineage_owner_allocation_charge(&self, prompt: &tau_proto::AgentPromptId) -> usize {
        let Some(dispatch) = self.inference_dispatches.get(prompt) else {
            return 0;
        };
        if let Some(owner) = &dispatch.checkpoint.output_length_continuation {
            return owned_value(owner);
        }
        let Some(transaction) = dispatch
            .checkpoint
            .transaction_id
            .as_ref()
            .and_then(|id| self.compaction_transactions.get(id))
        else {
            return 0;
        };
        let tau_proto::StandaloneCompactionTrigger::ReactiveContextOverflow {
            failed_agent_prompt_id,
        } = &transaction.started.trigger
        else {
            return 0;
        };
        self.inference_dispatches
            .get(failed_agent_prompt_id)
            .and_then(|failed| failed.checkpoint.output_length_continuation.as_ref())
            .map_or(0, owned_value)
    }

    /// Accounts for control-map insertions and replacement payloads
    /// individually.
    fn control_allocation_charge(&self, event: &Event) -> usize {
        let mut charge = 0usize;
        match event {
            Event::AgentManualCompactionRequested(requested) => {
                charge = map_growth(&self.manual_compaction_requests, 1)
                    .saturating_add(vec_growth(&self.manual_compaction_request_order, 1))
                    .saturating_add(owned_value(requested))
                    .saturating_add(owned_value(&requested.request_id).saturating_mul(2));
            }
            Event::AgentManualCompactionRequestFailed(failed) => {
                if self
                    .manual_compaction_requests
                    .contains_key(&failed.request_id)
                {
                    charge = owned_value(failed);
                }
            }
            Event::AgentManualCompactionRequestSatisfied(satisfied) => {
                if self
                    .manual_compaction_requests
                    .contains_key(&satisfied.request_id)
                {
                    charge = owned_value(&satisfied.transaction_id);
                }
            }
            Event::AgentStandaloneCompactionStarted(started) => {
                charge = map_growth(&self.compaction_transactions.entries, 1)
                    .saturating_add(vec_growth(&self.compaction_transactions.order, 1))
                    .saturating_add(owned_value(started))
                    .saturating_add(owned_value(&started.transaction_id).saturating_mul(2));
                use tau_proto::StandaloneCompactionTrigger as Trigger;
                let manual_states = match &started.trigger {
                    Trigger::ManualAgentTool { request_id, .. }
                    | Trigger::ManualUi { request_id } => {
                        usize::from(self.manual_compaction_requests.contains_key(request_id))
                    }
                    Trigger::AutomaticContextRetreat {
                        failed_transaction_id,
                        ..
                    }
                    | Trigger::AutomaticOutputLengthContinuation {
                        failed_transaction_id,
                    } => self
                        .manual_compaction_requests
                        .values()
                        .filter(|request| {
                            matches!(&request.state, ManualCompactionRequestState::Started(id)
                                if id == failed_transaction_id)
                        })
                        .count(),
                    _ => 0,
                };
                charge = charge.saturating_add(
                    owned_value(&started.transaction_id).saturating_mul(manual_states),
                );
            }
            Event::ProviderStandaloneExecutionAccounted(accounted) => {
                charge = map_growth(
                    &self.compaction_transactions.accounting,
                    usize::from(
                        !self
                            .compaction_transactions
                            .accounting
                            .contains_key(&accounted.agent_prompt_id),
                    ),
                )
                .saturating_add(owned_value(accounted))
                .saturating_add(owned_value(&accounted.agent_prompt_id));
            }
            Event::AgentStandaloneCompactionFailed(failed) => {
                if self
                    .compaction_transactions
                    .contains_key(&failed.transaction_id)
                {
                    charge = owned_value(failed);
                }
            }
            Event::AgentCompacted(compacted) => {
                if let Some(transaction) =
                    self.compaction_transactions.get(&compacted.transaction_id)
                {
                    charge = owned_value(compacted);
                    let mut additions = 0usize;
                    let mut superseded = transaction.started.supersedes.as_ref();
                    let mut remaining =
                        self.compaction_transactions.entries.len().saturating_add(1);
                    while let Some(id) = superseded {
                        if remaining == 0 {
                            return usize::MAX;
                        }
                        remaining -= 1;
                        // The initial/next link clone occurs before the insert
                        // and exists even when an already-resolved link stops
                        // it.
                        charge = charge.saturating_add(owned_value(id));
                        charge = charge.saturating_add(owned_value(id));
                        if self.resolved_compaction_failures.contains(id) {
                            break;
                        }
                        additions = additions.saturating_add(1);
                        superseded = self
                            .compaction_transactions
                            .get(id)
                            .and_then(|failed| failed.started.supersedes.as_ref());
                    }
                    if transaction.started.supersedes.is_some() {
                        charge = charge.saturating_add(owned_value(&transaction.started.model));
                        let through = transaction
                            .started
                            .resume_through
                            .unwrap_or(transaction.started.cut);
                        for (id, failed_transaction) in &self.compaction_transactions.entries {
                            if self.resolved_compaction_failures.contains(id)
                                || failed_transaction.started.model != transaction.started.model
                            {
                                continue;
                            }
                            let Some(CompactionTransactionOutcome::Failed(failed)) =
                                &failed_transaction.outcome
                            else {
                                continue;
                            };
                            if self.is_ancestor_head(failed.cut, through)
                                && failed
                                    .resume_through
                                    .is_none_or(|resume| self.is_ancestor_head(resume, through))
                            {
                                // Some may already occur in the supersedes
                                // chain;
                                // counting that intersection twice is
                                // conservative
                                // without allocating a temporary deduplication
                                // set.
                                additions = additions.saturating_add(1);
                                charge = charge.saturating_add(owned_value(&failed.transaction_id));
                            }
                        }
                    }
                    charge = charge
                        .saturating_add(set_growth(&self.resolved_compaction_failures, additions));
                }
            }
            Event::AgentInferenceDispatchStarted(checkpoint) => {
                charge = map_growth(&self.inference_dispatches, 1)
                    .saturating_add(vec_growth(&self.inference_dispatch_order, 1))
                    .saturating_add(owned_value(checkpoint))
                    .saturating_add(owned_value(&checkpoint.agent_prompt_id).saturating_mul(2));
                if checkpoint
                    .transaction_id
                    .as_ref()
                    .is_some_and(|id| self.compaction_transactions.contains_key(id))
                {
                    charge = charge.saturating_add(owned_value(checkpoint));
                }
            }
            Event::AgentPromptStarted(started) => {
                charge = map_growth(&self.prompt_starts, 1)
                    .saturating_add(owned_value(started))
                    .saturating_add(owned_value(&started.agent_prompt_id));
            }
            Event::ProviderResponseFinished(response) => {
                if self
                    .inference_dispatches
                    .contains_key(&response.agent_prompt_id)
                {
                    charge =
                        charge.saturating_add(owned_value(&response.output_length_disposition));
                }
                if response.standalone_rejection_reason().is_some() {
                    let copies = self
                        .compaction_transactions
                        .entries
                        .values()
                        .filter(|transaction| {
                            transaction.started.compact_prompt_id == response.agent_prompt_id
                        })
                        .count();
                    if copies != 0 {
                        charge =
                            charge.saturating_add(owned_value(response).saturating_mul(copies));
                    }
                }
                if let Some(decision) = &response.automatic_compaction_decision {
                    charge = charge.saturating_add(self.decision_allocation_charge(decision));
                }
            }
            Event::AgentPromptTerminated(terminated) => {
                if let Some(decision) = &terminated.automatic_compaction_decision {
                    charge = charge.saturating_add(self.decision_allocation_charge(decision));
                }
            }
            _ => {}
        }
        charge
    }

    /// Bounds one automatic decision payload and its map/order identities.
    fn decision_allocation_charge(
        &self,
        decision: &tau_proto::AutomaticCompactionDecision,
    ) -> usize {
        map_growth(&self.automatic_compaction_decisions, 1)
            .saturating_add(vec_growth(&self.automatic_compaction_decision_order, 1))
            .saturating_add(owned_value(decision))
            .saturating_add(owned_value(&decision.transaction_id).saturating_mul(2))
    }

    /// Mirrors only the allocating outputs of `output_length_dormant_repair`.
    ///
    /// Keep this borrowed selection beside that query when changing its states.
    /// Calling the public query here would allocate before its own reservation.
    fn dormant_repair_allocation_charge(&self) -> usize {
        let Some(active) = &self.active_outer_turn else {
            return 0;
        };
        let selected = self.head.map_or(AgentHead::Root, AgentHead::Node);
        let Some((source_id, source)) = self.inference_dispatch_order.iter().rev().find_map(|id| {
            let source = self.inference_dispatches.get(id)?;
            matches!(&source.output_length_disposition,
                tau_proto::OutputLengthDisposition::ContinuationPlanned {
                    outer_turn_id, ordinal: 1, limit: 1, ..
                } if outer_turn_id == active
            )
            .then_some((id, source))
        }) else {
            return 0;
        };
        let tau_proto::OutputLengthDisposition::ContinuationPlanned {
            outer_turn_id,
            successor_agent_prompt_id,
            ..
        } = &source.output_length_disposition
        else {
            return 0;
        };
        let Some(plan_node) = source.output_length_plan_node else {
            return 0;
        };
        if self.is_ancestor_head(AgentHead::Node(plan_node), selected) {
            return 0;
        }
        let source_charge = || {
            owned_value(&source.checkpoint)
                .saturating_add(owned_value(successor_agent_prompt_id))
                .saturating_add(owned_value(outer_turn_id))
        };
        if source.output_length_steer_node.is_none() {
            return source_charge();
        }
        let owner = self.inference_dispatches.values().find(|dispatch| {
            dispatch
                .checkpoint
                .output_length_continuation
                .as_ref()
                .is_some_and(|owner| &owner.source_agent_prompt_id == source_id)
        });
        let Some(owner) = owner else {
            return source_charge();
        };
        if !owner.finished {
            return if self
                .prompt_starts
                .contains_key(&owner.checkpoint.agent_prompt_id)
            {
                0
            } else {
                owned_value(&owner.checkpoint)
            };
        }
        let tau_proto::OutputLengthDisposition::ContinuationTerminal {
            outer_turn_id: terminal_turn,
            outcome: tau_proto::OutputLengthContinuationOutcome::Failed,
            outer_turn_finish_owed: true,
            ..
        } = &owner.output_length_disposition
        else {
            return 0;
        };
        if terminal_turn == outer_turn_id
            && self.outer_turn_is_open(outer_turn_id)
            && owner.response_node.is_some()
        {
            owned_value(outer_turn_id)
        } else {
            0
        }
    }
}

/// Charges ToolResponse's raw clone, rendered temporaries, headers and body.
fn tool_response_allocation_charge(value: &tau_proto::CborValue) -> usize {
    let mut charge = owned_value(value);
    let tau_proto::CborValue::Map(entries) = value else {
        return charge.saturating_add(tool_render_allocation_charge(value));
    };
    let slots = entries.len().saturating_mul(2);
    charge = charge
        .saturating_add(hash_growth::<(usize, String)>(0, 0, slots))
        .saturating_add(new_vec::<tau_proto::ToolResponseHeader>(entries.len()));
    let mut body_bytes = entries.len().saturating_mul(3);
    for (key, value) in entries {
        let key_bytes = tool_render_bytes(key);
        let value_bytes = tool_render_bytes(value);
        // cbor_tool_response_text requests a rendered String and a shape cache.
        // MaterializedMapProjection keeps a clone in rendered_text and returns
        // another clone for a header/body. Classification reuses that cache.
        charge = charge
            .saturating_add(tool_render_allocation_charge(key))
            .saturating_add(tool_render_allocation_charge(value))
            .saturating_add(key_bytes.saturating_add(value_bytes).saturating_mul(2));
        // Body labels sanitize already rendered key text; an ASCII control is
        // at most "\\u{7f}" (six bytes) and no Unicode scalar is worse per
        // byte.
        let label = key_bytes.saturating_mul(6);
        charge = charge.saturating_add(new_vec::<u8>(label));
        body_bytes = body_bytes.saturating_add(label).saturating_add(value_bytes);
    }
    charge.saturating_add(new_vec::<u8>(body_bytes))
}

/// Counts exact canonical rendering bytes through proto's nonallocating
/// measure.
fn tool_render_bytes(value: &tau_proto::CborValue) -> usize {
    tau_proto::measure_provider_tool_result_text(
        value,
        tau_proto::ProviderToolResultStatus::Success,
    )
    .rendered_bytes
}

/// Bounds one rendering buffer and its per-node shape-cache backing table.
fn tool_render_allocation_charge(value: &tau_proto::CborValue) -> usize {
    // ProviderTextShape has five usize counters, one bool and three signatures;
    // each signature contains at most 21 bytes plus its usize length. Six
    // counters below leave one extra word while preserving alignment.
    type ShapeLayoutBound = ([usize; 6], bool, [([u8; 21], usize); 3]);
    new_vec::<u8>(tool_render_bytes(value)).saturating_add(
        hash_growth::<(usize, ShapeLayoutBound)>(0, 0, cbor_nodes(value)),
    )
}

/// Counts raw CBOR nodes without materializing or serializing them.
fn cbor_nodes(value: &tau_proto::CborValue) -> usize {
    use tau_proto::CborValue;
    match value {
        CborValue::Array(items) => items
            .iter()
            .fold(1usize, |n, item| n.saturating_add(cbor_nodes(item))),
        CborValue::Map(entries) => entries.iter().fold(1usize, |n, (key, value)| {
            n.saturating_add(cbor_nodes(key))
                .saturating_add(cbor_nodes(value))
        }),
        CborValue::Tag(_, value) => 1usize.saturating_add(cbor_nodes(value)),
        _ => 1,
    }
}

/// Counts raw-message rendering, including intermediate XML replacement
/// buffers.
fn message_projection_charge(event: &Event) -> usize {
    // Keep the field selection aligned with MessageFactView; extension-private
    // CBOR is deliberately absent because projection never clones or renders
    // it.
    let (publisher, agent, message_id, reference, party, conversation, text, reaction) = match event
    {
        Event::MessageDelivered(value) => (
            value.publisher_extension_id.as_str(),
            value.agent_id.as_str(),
            Some(value.message_id.as_str()),
            None,
            Some(&value.sender),
            value.conversation.as_ref(),
            Some(value.text.as_str()),
            None,
        ),
        Event::MessageSent(value) => (
            value.publisher_extension_id.as_str(),
            value.agent_id.as_str(),
            Some(value.message_id.as_str()),
            None,
            value.recipient.as_ref(),
            value.conversation.as_ref(),
            Some(value.text.as_str()),
            None,
        ),
        Event::MessageEdited(value) => (
            value.publisher_extension_id.as_str(),
            value.agent_id.as_str(),
            None,
            Some(&value.target),
            value.actor.as_ref(),
            value.conversation.as_ref(),
            Some(value.text.as_str()),
            None,
        ),
        Event::MessageDeleted(value) => (
            value.publisher_extension_id.as_str(),
            value.agent_id.as_str(),
            None,
            Some(&value.target),
            value.actor.as_ref(),
            value.conversation.as_ref(),
            None,
            None,
        ),
        Event::MessageReactionAdded(value) => (
            value.publisher_extension_id.as_str(),
            value.agent_id.as_str(),
            None,
            Some(&value.target),
            value.actor.as_ref(),
            value.conversation.as_ref(),
            None,
            Some(value.reaction.as_str()),
        ),
        Event::MessageReactionRemoved(value) => (
            value.publisher_extension_id.as_str(),
            value.agent_id.as_str(),
            None,
            Some(&value.target),
            value.actor.as_ref(),
            value.conversation.as_ref(),
            None,
            Some(value.reaction.as_str()),
        ),
        _ => return 0,
    };
    let mut charge = agent.len();
    // Fixed syntax, event/auth/trust names and temporary attribute-name formats
    // are below 1 KiB together in the current render_message_fact
    // implementation.
    let mut output_bytes = 1024usize;
    let mut attribute = |value: &str| {
        let visible = visible_bytes(value);
        let escaped = value.chars().fold(visible, |bytes, ch| {
            bytes.saturating_add(match ch {
                '&' => 4,
                '<' | '>' => 3,
                '"' | '\'' => 5,
                _ => 0,
            })
        });
        output_bytes = output_bytes.saturating_add(escaped);
        // visible_escape_metadata followed by five chained String::replace
        // calls. Each intermediate is no larger than the final escaped value.
        charge = charge.saturating_add(new_vec::<u8>(escaped).saturating_mul(6));
    };
    attribute(publisher);
    if let Some(id) = message_id {
        attribute(id);
    }
    if let Some(reference) = reference {
        attribute(reference.publisher_extension_id.as_str());
        attribute(reference.message_id.as_str());
    }
    if let Some(party) = party {
        attribute(&party.stable_id);
        if let Some(display) = &party.display_name {
            attribute(display);
        }
    }
    if let Some(alias) = conversation.and_then(|value| value.alias.as_deref()) {
        attribute(alias);
    }
    if let Some(reaction) = reaction {
        attribute(reaction);
    }
    if let Some(text) = text {
        let visible = visible_bytes(text);
        let envelope = tau_proto::MESSAGE_PAYLOAD_ENVELOPE;
        // Visible escaping cannot manufacture or destroy this ASCII sentinel.
        let matches = text.matches(envelope.exact_close).count();
        let escaped = visible.saturating_add(
            matches.saturating_mul(
                envelope
                    .visible_close
                    .len()
                    .saturating_sub(envelope.exact_close.len()),
            ),
        );
        charge = charge.saturating_add(new_vec::<u8>(visible));
        if matches != 0 {
            charge = charge.saturating_add(new_vec::<u8>(escaped));
        }
        output_bytes = output_bytes.saturating_add(escaped);
    }
    charge
        .saturating_add(new_vec::<u8>(output_bytes))
        .saturating_add(new_vec::<ContentPart>(1))
        .saturating_add(size_of::<tau_proto::MessageItem>())
}

/// Counts exactly the Unicode escapes produced by visible_escape_metadata.
fn visible_bytes(text: &str) -> usize {
    text.chars().fold(0usize, |bytes, ch| {
        let width = if tau_proto::requires_visible_escape(ch) {
            // "\\u{" + at least four hexadecimal digits + "}".
            let scalar = ch as u32;
            4 + if 0xfffff < scalar {
                6
            } else if 0xffff < scalar {
                5
            } else {
                4
            }
        } else {
            ch.len_utf8()
        };
        bytes.saturating_add(width)
    })
}
