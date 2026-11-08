use super::*;

fn streaming_response(main: &AgentId) -> tau_proto::ProviderResponseFinished {
    let mut response =
        tool_calling_response(main, "ap-stream", vec!["stream".into(), "sibling".into()]);
    response.streamed_tool_items = vec![0];
    response
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
    let call = tree
        .tool_output_call(&"stream".into(), tree.head())
        .expect("call");
    let page = tree
        .tool_output_page(call, tree.head(), 0)
        .expect("deferred final");
    assert!(page.text.contains("omitted_bytes=\"17\""));
    assert!(page.next_offset.is_some());
    let cold = AgentTree::try_from_events(main, &records).expect("cold capture");
    assert_eq!(
        cold.tool_output_page(call, cold.head(), 0)
            .expect("cold page"),
        page
    );
}

#[test]
fn display_id_recovery_selects_the_latest_declaration_on_its_branch() {
    let main = agent_id();
    let mut tree = AgentTree::from_events(main.clone(), &[]);
    let mut records = Vec::new();
    let mut branches = Vec::new();
    for text in ["left-output", "right-output"] {
        append(
            &mut tree,
            &mut records,
            Event::ProviderResponseFinished(streaming_response(&main)),
        );
        let call = tree
            .tool_output_call(&"stream".into(), tree.head())
            .expect("call");
        append(&mut tree, &mut records, update(call, 0, text));
        for id in ["stream", "sibling"] {
            append(
                &mut tree,
                &mut records,
                Event::ProviderToolResult(tau_proto::ToolResult {
                    captured_output: None,
                    call_id: id.into(),
                    tool_name: ToolName::new("tool"),
                    tool_type: ToolType::Function,
                    result: tau_proto::CborValue::Null,
                    presentation: if id == "stream" {
                        tau_proto::ToolResultPresentation::ToolStreamReceipt
                    } else {
                        Default::default()
                    },
                    provider_content: Vec::new(),
                    kind: ToolResultKind::Final,
                    display: None,
                    originator: PromptOriginator::User,
                }),
            );
        }
        branches.push((tree.head(), call, text));
        append(
            &mut tree,
            &mut records,
            Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
                agent_id: main.clone(),
                head: tau_proto::AgentHead::Root,
            }),
        );
    }
    let cold = AgentTree::try_from_events(main, &records).expect("cold branches");
    for (head, call, text) in branches {
        for tree in [&tree, &cold] {
            assert_eq!(tree.tool_output_call(&"stream".into(), head), Some(call));
            let page = tree.tool_output_page(call, head, 0).expect("branch page");
            assert!(page.text.contains(text));
            assert!(tree.tool_output_page(call, None, 0).is_err());
        }
    }
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
    let sibling = tree
        .tool_output_call(&"sibling".into(), tree.head())
        .expect("sibling");
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
        let call = tree
            .tool_output_call(&"stream".into(), tree.head())
            .expect("stream");
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

/// Recovery offsets count retained canonical UTF-8 bytes, not producer bytes or
/// chars. Fixed-size pages neither split scalars nor lose cross-frame suffixes.
#[test]
fn tool_output_utf8_pages_match_cold_deferred_capture() {
    let main = agent_id();
    let mut tree = AgentTree::from_events(main.clone(), &[]);
    let mut records = Vec::new();
    append(
        &mut tree,
        &mut records,
        Event::ProviderResponseFinished(streaming_response(&main)),
    );
    let call = tree
        .tool_output_call(&"stream".into(), tree.head())
        .expect("stream");
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
    let mut offset = 0;
    let mut aggregate = String::new();
    loop {
        let live = tree
            .tool_output_page(call, tree.head(), offset)
            .expect("live page");
        assert_eq!(
            cold.tool_output_page(call, cold.head(), offset)
                .expect("cold page"),
            live
        );
        assert!(live.text.len() <= tau_proto::MAX_TOOL_OUTPUT_CHUNK_BYTES);
        aggregate.push_str(&live.text);
        if let Some(next) = live.next_offset {
            assert!(next > offset);
            assert_eq!(next, aggregate.len());
            offset = next;
        } else {
            assert_eq!(aggregate.len(), live.retained_bytes);
            break;
        }
    }
    assert_eq!(aggregate.matches('é').count(), 4096);
    assert_eq!(aggregate.matches('🦀').count(), 2048);
    assert_eq!(aggregate.matches("final incremental").count(), 1);
    let first_accent = aggregate.find('é').expect("accent");
    assert!(
        tree.tool_output_page(call, tree.head(), first_accent + 1)
            .is_err()
    );
    assert!(
        tree.tool_output_page(call, tree.head(), aggregate.len() + 1)
            .is_err()
    );
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
    let call = tree
        .tool_output_call(&"stream".into(), tree.head())
        .expect("declared call");
    append(&mut tree, &mut records, update(call, 0, "first-unique"));
    append(&mut tree, &mut records, update(call, 1, "second-unique"));
    let page = tree
        .tool_output_page(call, tree.head(), 0)
        .expect("deferred recovery");
    assert!(page.text.contains("first-unique"));
    assert!(page.text.find("first-unique") < page.text.find("second-unique"));
    assert!(!page.terminal);
    assert!(
        tree.node_for_durable_event_seq(PersistedAgentEventSeq::new(1))
            .is_none()
    );
    let cold = AgentTree::try_from_events(main.clone(), &records).expect("cold open round");
    assert_eq!(
        cold.tool_output_page(call, cold.head(), 0)
            .expect("cold page"),
        page
    );

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
    let page = tree
        .tool_output_page(call, tree.head(), 0)
        .expect("closed recovery");
    assert!(page.terminal);
    assert_eq!(page.text.matches("last-unique").count(), 1);
    assert!(page.text.find("second-unique") < page.text.find("last-unique"));
    let cold = AgentTree::try_from_events(main, &records).expect("cold closed round");
    assert_eq!(
        cold.tool_output_page(call, cold.head(), 0)
            .expect("cold page"),
        page
    );
    let result = tree
        .terminal_tool_result_at(
            *tree
                .terminal_tool_result_nodes
                .get("stream")
                .expect("result node"),
            &"stream".into(),
        )
        .expect("native receipt");
    assert!(result.output.render().contains("outcome"));
    assert!(!result.output.render().contains("last-unique"));
    assert!(tree.validate_event(&update(call, 2, "late")).is_err());
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
    let call = tree
        .tool_output_call(&"stream".into(), tree.head())
        .expect("stream");
    let sibling = tree
        .tool_output_call(&"sibling".into(), tree.head())
        .expect("sibling");
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
