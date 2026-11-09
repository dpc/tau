use super::*;

/// Canonical source projected into materialized or deferred context.
#[derive(Debug, PartialEq)]
struct AcceptedSource {
    /// Complete accepted envelope text, with no paging or byte cursor.
    text: String,
    /// Whether a canonical terminal closed the declaration.
    terminal: bool,
}

impl AgentTree {
    /// Select the declaration created by the test's current foreground round.
    fn declared_call(&self, id: &str) -> tau_proto::ToolCallRef {
        self.indexes.tool_output_calls[id].to_owned()
    }

    /// Assert accepted context directly, including both deferred input queues.
    fn accepted_source(&self, call: tau_proto::ToolCallRef) -> AcceptedSource {
        let state = &self.indexes.tool_outputs[&call];
        let mut text = String::new();
        for seq in &state.occurrences {
            if let Some(input) = self
                .pending_context_inputs
                .iter()
                .find(|input| input.durable_event_seq == *seq)
                && let PendingToolContextEntry::Output(update) = &input.entry
            {
                text.push_str(&update.render());
                continue;
            }
            let entry = self
                .node_for_durable_event_seq(*seq)
                .and_then(|id| self.node(id))
                .map(|node| &node.entry)
                .or_else(|| {
                    self.pending_context_inputs
                        .iter()
                        .find(|input| input.durable_event_seq == *seq)
                        .and_then(|input| input.entry.context())
                })
                .or_else(|| {
                    self.pending_inference_inputs
                        .iter()
                        .find(|input| input.durable_event_seq == *seq)
                        .map(|input| input.entry.as_ref())
                })
                .expect("accepted occurrence must have context");
            let AgentEntry::UserInput { items, .. } = entry else {
                panic!("accepted source must be user context");
            };
            for item in items {
                if let ContextItem::Message(message) = item {
                    for part in &message.content {
                        if let ContentPart::Text { text: body, .. } = part {
                            text.push_str(body);
                        }
                    }
                }
            }
        }
        AcceptedSource {
            text,
            terminal: state.terminal,
        }
    }
}

fn streaming_response(main: &AgentId) -> tau_proto::ProviderResponseFinished {
    let mut response =
        tool_calling_response(main, "ap-stream", vec!["stream".into(), "sibling".into()]);
    response.streamed_tool_items = vec![0];
    response
}

/// Build a new harness-owned native terminal without changing historical
/// receipt fixtures below.
fn native_result(id: &str, text: &str) -> Event {
    Event::ProviderToolResult(tau_proto::ToolResult {
        call_id: id.into(),
        tool_name: ToolName::new("tool"),
        tool_type: ToolType::Function,
        result: tau_proto::CborValue::Null,
        captured_output: Some(tau_proto::ToolFinalOutput::capture_native(
            &tau_proto::CborValue::Text(text.to_owned()),
            tau_proto::ProviderToolResultStatus::Success,
        )),
        presentation: tau_proto::ToolResultPresentation::NativeCapturedOutput,
        provider_content: Vec::new(),
        kind: ToolResultKind::Final,
        originator: PromptOriginator::User,
        display: None,
    })
}

/// Foreground capture absorbs zero, multiple, and maximum-count batches without
/// exposing source early or re-capping accepted bytes. Both sibling completion
/// orders, foreign deferred input, and every cold cut preserve exact custody.
#[test]
fn native_foreground_source_matches_cold_cuts_and_preserves_deferred_order() {
    for count in [0, 2, 64] {
        for sibling_first in [false, true] {
            let main = agent_id();
            let mut tree = AgentTree::from_events(main.clone(), &[]);
            let mut records = Vec::new();
            let mut response = streaming_response(&main);
            response.streamed_tool_items = vec![0, 1];
            append(
                &mut tree,
                &mut records,
                Event::ProviderResponseFinished(response),
            );
            let call = tree.declared_call("stream");
            let sibling = tree.declared_call("sibling");
            let mut source_seqs = Vec::new();
            for sequence in 0..count {
                source_seqs.push(tree.next_event_seq());
                let mut batch = update(
                    call,
                    sequence,
                    &format!("batch-{sequence}-λ{}", "x".repeat(990)),
                );
                if let Event::ToolOutputUpdated(update) = &mut batch {
                    update.output.omitted_bytes = sequence + 1;
                }
                append(&mut tree, &mut records, batch);
                let cold = AgentTree::try_from_events(main.clone(), &records).expect("cold update");
                assert_eq!(tree.pending_context_inputs, cold.pending_context_inputs);
                assert!(cold.tool_output_needs_residency(call));
                assert!(
                    cold.node_for_durable_event_seq(*source_seqs.last().unwrap())
                        .is_none()
                );
            }
            append(&mut tree, &mut records, prompt_event(main.clone()));
            let prompt_seq = records.last().unwrap().seq;
            let mut sibling_update = update(sibling, 0, "sibling-partial");
            if let Event::ToolOutputUpdated(update) = &mut sibling_update {
                update.output.call_id = "sibling".into();
            }
            append(&mut tree, &mut records, sibling_update);
            if sibling_first {
                append(
                    &mut tree,
                    &mut records,
                    native_result("sibling", "sibling-tail"),
                );
            }
            source_seqs.push(tree.next_event_seq());
            append(
                &mut tree,
                &mut records,
                native_result("stream", "final-tail"),
            );
            assert!(tree.tool_output_is_native(call));
            let cold = AgentTree::try_from_events(main.clone(), &records).expect("cold native");
            assert_eq!(tree.pending_context_inputs, cold.pending_context_inputs);
            assert_eq!(tree.tool_output_needs_residency(call), !sibling_first);
            assert_eq!(cold.tool_output_needs_residency(call), !sibling_first);
            for seq in &source_seqs {
                assert!(cold.tool_output_occurrence_is_native(call, *seq));
            }
            if !sibling_first {
                append(
                    &mut tree,
                    &mut records,
                    native_result("sibling", "sibling-tail"),
                );
            }
            let node = tree
                .node_for_durable_event_seq(source_seqs[0])
                .expect("native source node");
            for seq in &source_seqs {
                assert_eq!(tree.node_for_durable_event_seq(*seq), Some(node));
            }
            let AgentEntry::ToolResults { items } = &tree.node(node).unwrap().entry else {
                panic!("native source must be the result aggregate");
            };
            let output = items[0].render_provider_text();
            assert_eq!(output.matches("final-tail").count(), 1);
            for sequence in 0..count {
                assert_eq!(output.matches(&format!("batch-{sequence}-λ")).count(), 1);
            }
            assert!(!output.contains("sibling"));
            assert!(!output.contains("<tool_output"));
            assert!(!output.contains("outcome: completed"));
            assert_eq!(
                items[1]
                    .render_provider_text()
                    .matches("sibling-partial")
                    .count(),
                1
            );
            assert_eq!(
                tree.node(tree.node_for_durable_event_seq(prompt_seq).unwrap())
                    .unwrap()
                    .parent_id,
                Some(node)
            );
            assert!(!tree.tool_output_needs_residency(call));
            assert!(!tree.tool_output_needs_residency(sibling));
            let cold = AgentTree::try_from_events(main, &records).expect("cold closed");
            assert_eq!(
                tree.active_provider_window(tree.head()).transcript,
                cold.active_provider_window(cold.head()).transcript
            );
        }
    }
}

/// Native errors and all cancellation-tail states preserve real protocol
/// outcomes and render one diagnostic, not a successful completion receipt.
#[test]
fn native_failure_source_preserves_status_and_available_empty_cancellation() {
    for tail in [None, Some(""), Some("cancel-tail")] {
        let main = agent_id();
        let mut tree = AgentTree::from_events(main.clone(), &[]);
        let mut records = Vec::new();
        let mut response = streaming_response(&main);
        response.streamed_tool_items = vec![0, 1];
        append(
            &mut tree,
            &mut records,
            Event::ProviderResponseFinished(response),
        );
        let call = tree.declared_call("stream");
        append(
            &mut tree,
            &mut records,
            update(call, 0, "accepted-before-cancel"),
        );
        let reason = if tail.is_some() {
            "tool cancelled"
        } else {
            "final capture unavailable"
        };
        let value = tail.map_or(tau_proto::CborValue::Null, |text| {
            tau_proto::CborValue::Text(text.to_owned())
        });
        append(
            &mut tree,
            &mut records,
            Event::ToolCancelled(tau_proto::ToolCancelled {
                call_id: "stream".into(),
                tool_name: ToolName::new("tool"),
                tool_type: ToolType::Function,
                output: None,
                captured_output: Some(tau_proto::ToolFinalOutput::capture_native(
                    &value,
                    tau_proto::ProviderToolResultStatus::Cancelled { reason },
                )),
                presentation: tau_proto::ToolResultPresentation::NativeCapturedOutput,
                display: None,
            }),
        );
        append(
            &mut tree,
            &mut records,
            Event::ProviderToolError(tau_proto::ToolError {
                call_id: "sibling".into(),
                tool_name: ToolName::new("tool"),
                tool_type: ToolType::Function,
                message: "native-error".to_owned(),
                details: None,
                captured_output: Some(tau_proto::ToolFinalOutput::capture_native(
                    &tau_proto::CborValue::Text("error-tail".to_owned()),
                    tau_proto::ProviderToolResultStatus::Error {
                        message: "native-error",
                    },
                )),
                presentation: tau_proto::ToolResultPresentation::NativeCapturedOutput,
                display: None,
                originator: PromptOriginator::User,
            }),
        );
        let node = tree.node_for_durable_event_seq(records[1].seq).unwrap();
        let AgentEntry::ToolResults { items } = &tree.node(node).unwrap().entry else {
            panic!("native aggregate");
        };
        assert!(matches!(
            items[0].status,
            ToolResultStatus::Cancelled { .. }
        ));
        assert!(matches!(items[1].status, ToolResultStatus::Error { .. }));
        let text = items[0].render_provider_text();
        assert_eq!(text.matches("cancelled:").count(), 1);
        assert!(text.contains(reason));
        assert_eq!(text.contains("final capture unavailable"), tail.is_none());
        assert_eq!(text.matches("accepted-before-cancel").count(), 1);
        if tail == Some("cancel-tail") {
            assert_eq!(text.matches("cancel-tail").count(), 1);
        }
        assert_eq!(
            items[1].render_provider_text(),
            "error: native-error\n\nerror-tail"
        );
        let cold = AgentTree::try_from_events(main, &records).expect("cold failures");
        assert_eq!(
            tree.active_provider_window(tree.head()).transcript,
            cold.active_provider_window(cold.head()).transcript
        );
    }
}

/// Admission must charge the owned final source even when its old raw payload
/// has been replaced with tiny process metadata.
#[test]
fn final_capture_is_bounded_and_charged_before_deferred_projection() {
    let main = agent_id();
    let mut tree = AgentTree::from_events(main.clone(), &[]);
    let mut records = Vec::new();
    append(
        &mut tree,
        &mut records,
        Event::ProviderResponseFinished(streaming_response(&main)),
    );
    let record = |text: String| PersistedAgentEvent {
        observation_id: tau_proto::ObservationId::from_bytes([44; 16]),
        seq: tree.next_event_seq(),
        source: None,
        parent: AgentEventParent::InheritHead,
        fold_semantics: AgentJournalFoldSemantics::CommitOrder,
        recorded_at: UnixMicros::default(),
        event: Event::ProviderToolResult(tau_proto::ToolResult {
            captured_output: Some(tau_proto::ToolFinalOutput {
                outcome: None,
                text,
                omitted_bytes: 17,
            }),
            call_id: "stream".into(),
            tool_name: ToolName::new("tool"),
            tool_type: ToolType::Function,
            result: tau_proto::CborValue::Null,
            presentation: tau_proto::ToolResultPresentation::ToolStreamReceipt,
            provider_content: Vec::new(),
            kind: ToolResultKind::Final,
            display: None,
            originator: PromptOriginator::User,
        }),
    };
    let small = record("x".into());
    let large = record("x".repeat(tau_proto::MAX_TOOL_OUTPUT_FINAL_BYTES));
    let oversized = record("x".repeat(tau_proto::MAX_TOOL_OUTPUT_FINAL_BYTES + 1));
    assert!(tree.validate_event(&oversized.event).is_err());
    assert!(
        tree.fold_allocation_charge(&large)
            >= tree.fold_allocation_charge(&small)
                + 4 * (tau_proto::MAX_TOOL_OUTPUT_FINAL_BYTES - 1),
        "capture storage and its projected source must be reserved, not just raw null"
    );
    tree.apply_persisted_record(&large).expect("bounded final");
    records.push(large);
    let call = tree.declared_call("stream");
    let page = tree.accepted_source(call);
    assert!(page.text.contains("omitted_bytes=\"17\""));
    assert!(page.text.len() > tau_proto::MAX_TOOL_OUTPUT_FINAL_BYTES);
    let cold = AgentTree::try_from_events(main, &records).expect("cold capture");
    assert_eq!(cold.accepted_source(call), page);
}

/// Declaration-side authority is exact, ordered and durable even before any
/// frame; malformed indices and a plain sibling never authorize source output.
#[test]
fn streamed_policy_rejects_invalid_indices_and_non_opted_sibling() {
    let main = agent_id();
    let mut tree = AgentTree::from_events(main.clone(), &[]);
    for indices in [vec![0, 0], vec![1, 0], vec![2], vec![u32::MAX]] {
        let mut response = streaming_response(&main);
        response.streamed_tool_items = indices;
        assert!(
            tree.validate_event(&Event::ProviderResponseFinished(response))
                .is_err()
        );
    }
    let mut records = Vec::new();
    append(
        &mut tree,
        &mut records,
        Event::ProviderResponseFinished(streaming_response(&main)),
    );
    let sibling = tree.declared_call("sibling");
    let mut event = update(sibling, 0, "not opted in");
    let Event::ToolOutputUpdated(value) = &mut event else {
        unreachable!()
    };
    value.output.call_id = "sibling".into();
    assert!(!tree.tool_output_is_streamed(sibling));
    assert!(tree.validate_event(&event).is_err());
}

/// Cold repair follows the global foreground-round invariant, but an abandoned
/// off-branch background placeholder is historical data, not a running capture.
#[test]
fn cold_output_debt_preserves_off_branch_foreground_but_not_historical_background() {
    for with_output in [false, true] {
        let main = agent_id();
        let mut tree = AgentTree::from_events(main.clone(), &[]);
        let mut records = Vec::new();
        append(
            &mut tree,
            &mut records,
            Event::ProviderResponseFinished(streaming_response(&main)),
        );
        let call = tree.declared_call("stream");
        if with_output {
            append(
                &mut tree,
                &mut records,
                update(call, 0, "off-branch retained bytes"),
            );
        }
        append(
            &mut tree,
            &mut records,
            Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
                agent_id: main.clone(),
                head: tau_proto::AgentHead::Root,
            }),
        );
        let cold =
            AgentTree::try_from_events(main.clone(), &records).expect("cold off-branch foreground");
        assert_eq!(
            cold.pending_tool_output_residencies().collect::<Vec<_>>(),
            vec![call]
        );
        for (id, kind) in [
            ("stream", ToolResultKind::BackgroundPlaceholder),
            ("sibling", ToolResultKind::Final),
        ] {
            append(
                &mut tree,
                &mut records,
                Event::ProviderToolResult(tau_proto::ToolResult {
                    captured_output: None,
                    presentation: Default::default(),
                    call_id: id.into(),
                    tool_name: ToolName::new("tool"),
                    tool_type: ToolType::Function,
                    result: tau_proto::CborValue::Text("native closure".to_owned()),
                    provider_content: Vec::new(),
                    kind,
                    display: None,
                    originator: PromptOriginator::User,
                }),
            );
        }
        let cold = AgentTree::try_from_events(main, &records).expect("cold historical background");
        assert!(
            cold.tool_output_needs_residency(call),
            "a live process would still own custody"
        );
        assert!(
            cold.pending_tool_output_residencies().next().is_none(),
            "restart does not resume an off-branch background process"
        );
    }
}

/// Deferred context preserves exact UTF-8 source across frames and cold replay.
#[test]
fn tool_output_utf8_matches_cold_deferred_context() {
    let main = agent_id();
    let mut tree = AgentTree::from_events(main.clone(), &[]);
    let mut records = Vec::new();
    append(
        &mut tree,
        &mut records,
        Event::ProviderResponseFinished(streaming_response(&main)),
    );
    let call = tree.declared_call("stream");
    for (sequence, text) in [
        "é".repeat(4096),
        "🦀".repeat(2048),
        "final incremental".to_owned(),
    ]
    .iter()
    .enumerate()
    {
        append(&mut tree, &mut records, update(call, sequence as u64, text));
    }
    let cold = AgentTree::try_from_events(main, &records).expect("cold deferred capture");
    let live = tree.accepted_source(call);
    assert_eq!(cold.accepted_source(call), live);
    let aggregate = live.text;
    assert_eq!(aggregate.matches('é').count(), 4096);
    assert_eq!(aggregate.matches('🦀').count(), 2048);
    assert_eq!(aggregate.matches("final incremental").count(), 1);
}

fn append(tree: &mut AgentTree, records: &mut Vec<PersistedAgentEvent>, event: Event) {
    let record = PersistedAgentEvent {
        observation_id: tau_proto::ObservationId::from_bytes([records.len() as u8; 16]),
        seq: tree.next_event_seq(),
        source: None,
        event,
        parent: AgentEventParent::InheritHead,
        fold_semantics: AgentJournalFoldSemantics::CommitOrder,
        recorded_at: UnixMicros::default(),
    };
    tree.apply_persisted_record(&record).expect("live append");
    records.push(record);
}

fn update(call: tau_proto::ToolCallRef, sequence: u64, text: &str) -> Event {
    Event::ToolOutputUpdated(tau_proto::ToolOutputUpdate {
        call,
        output: tau_proto::ToolOutputReport {
            call_id: "stream".into(),
            tool_name: ToolName::new("tool"),
            sequence,
            text: text.to_owned(),
            omitted_bytes: 0,
        },
    })
}

/// Accepted source frames survive deferred placement and cold replay; closing
/// the native round appends only a receipt, not another copy of the source
/// tail.
#[test]
fn ordered_tool_output_and_recovery_match_live_and_cold_deferred_round() {
    let main = agent_id();
    let mut tree = AgentTree::from_events(main.clone(), &[]);
    let mut records = Vec::new();
    append(
        &mut tree,
        &mut records,
        Event::ProviderResponseFinished(streaming_response(&main)),
    );
    let call = tree.declared_call("stream");
    append(&mut tree, &mut records, update(call, 0, "first-unique"));
    append(&mut tree, &mut records, update(call, 1, "second-unique"));
    let page = tree.accepted_source(call);
    assert!(page.text.contains("first-unique"));
    assert!(page.text.find("first-unique") < page.text.find("second-unique"));
    assert!(!page.terminal);
    assert!(
        tree.node_for_durable_event_seq(PersistedAgentEventSeq::new(1))
            .is_none()
    );
    let cold = AgentTree::try_from_events(main.clone(), &records).expect("cold open round");
    assert_eq!(cold.accepted_source(call), page);

    for (id, presentation, text) in [
        (
            "stream",
            tau_proto::ToolResultPresentation::ToolStreamReceipt,
            "last-unique",
        ),
        (
            "sibling",
            tau_proto::ToolResultPresentation::default(),
            "other",
        ),
    ] {
        append(
            &mut tree,
            &mut records,
            Event::ProviderToolResult(tau_proto::ToolResult {
                captured_output: None,
                presentation,
                call_id: id.into(),
                tool_name: ToolName::new("tool"),
                tool_type: ToolType::Function,
                result: tau_proto::CborValue::Text(text.to_owned()),
                provider_content: Vec::new(),
                kind: ToolResultKind::Final,
                display: None,
                originator: PromptOriginator::User,
            }),
        );
    }
    let page = tree.accepted_source(call);
    assert!(page.terminal);
    assert_eq!(page.text.matches("last-unique").count(), 1);
    assert!(page.text.find("second-unique") < page.text.find("last-unique"));
    let cold = AgentTree::try_from_events(main, &records).expect("cold closed round");
    assert_eq!(cold.accepted_source(call), page);
    let result = tree
        .terminal_tool_result_at(
            *tree
                .terminal_tool_result_nodes
                .get("stream")
                .expect("result node"),
            &"stream".into(),
        )
        .expect("native receipt");
    assert_eq!(result.output.render(), "outcome: completed");
    assert!(!result.output.render().contains("last-unique"));
    assert!(tree.validate_event(&update(call, 2, "late")).is_err());
}

/// Failed and cancelled streamed calls deliver their source once, then close
/// successfully with only an outcome; cold replay must preserve that split.
#[test]
fn failed_stream_terminals_omit_self_cause_without_losing_final_source() {
    for cancelled in [false, true] {
        let main = agent_id();
        let mut tree = AgentTree::from_events(main.clone(), &[]);
        let mut records = Vec::new();
        let mut response = streaming_response(&main);
        response.output_items.truncate(1);
        append(
            &mut tree,
            &mut records,
            Event::ProviderResponseFinished(response),
        );
        let call = tree.declared_call("stream");
        let capture = Some(tau_proto::ToolFinalOutput {
            outcome: None,
            text: "terminal-source-unique".into(),
            omitted_bytes: 0,
        });
        let terminal = if cancelled {
            Event::ToolCancelled(tau_proto::ToolCancelled {
                output: Some(String::new()),
                captured_output: capture,
                call_id: "stream".into(),
                tool_name: ToolName::new("tool"),
                tool_type: ToolType::Function,
                presentation: tau_proto::ToolResultPresentation::ToolStreamReceipt,
                display: None,
            })
        } else {
            Event::ProviderToolError(tau_proto::ToolError {
                captured_output: capture,
                call_id: "stream".into(),
                tool_name: ToolName::new("tool"),
                tool_type: ToolType::Function,
                message: "failed".into(),
                details: Some(tau_proto::CborValue::Text(String::new())),
                presentation: tau_proto::ToolResultPresentation::ToolStreamReceipt,
                display: None,
                originator: PromptOriginator::User,
            })
        };
        append(&mut tree, &mut records, terminal);
        let cold = AgentTree::try_from_events(main, &records).expect("cold terminal");
        for tree in [&tree, &cold] {
            let node = *tree
                .terminal_tool_result_nodes
                .get("stream")
                .expect("terminal");
            let receipt = tree
                .terminal_tool_result_at(node, &"stream".into())
                .expect("receipt");
            assert_eq!(receipt.status, ToolResultStatus::Success);
            assert_eq!(
                receipt.output.render(),
                if cancelled {
                    "outcome: cancelled"
                } else {
                    "outcome: error"
                },
            );
            let page = tree.accepted_source(call);
            assert!(page.terminal);
            assert_eq!(page.text.matches("terminal-source-unique").count(), 1);
        }
    }
}

/// Display-ID reuse, a sibling declaration, gaps, and empty heartbeats cannot
/// manufacture an accepted source occurrence.
#[test]
fn tool_output_rejects_wrong_identity_gap_and_empty_heartbeat() {
    let main = agent_id();
    let mut tree = AgentTree::from_events(main.clone(), &[]);
    let mut records = Vec::new();
    append(
        &mut tree,
        &mut records,
        Event::ProviderResponseFinished(streaming_response(&main)),
    );
    let call = tree.declared_call("stream");
    let sibling = tree.declared_call("sibling");
    for event in [
        update(sibling, 0, "wrong"),
        update(call, 1, "gap"),
        update(call, 0, ""),
    ] {
        assert!(tree.validate_event(&event).is_err());
    }
    append(&mut tree, &mut records, update(call, 0, "valid"));
    assert!(tree.validate_event(&update(call, 0, "duplicate")).is_err());
}
