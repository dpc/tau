use super::*;

/// Provider compaction items form canonical replacement context, while harness
/// triggers and private local envelopes must never become durable replacements.
#[test]
fn compaction_window_accepts_provider_item_and_rejects_harness_trigger() {
    let provider_item = ContextItem::Compaction(
        OpaqueProviderItem::from_raw_json(
            r#"{"type":"compaction","id":"cmp_1","encrypted_content":"opaque"}"#,
        )
        .expect("valid opaque compaction"),
    );

    assert!(ValidatedCompactionWindow::new(vec![provider_item]).is_ok());
    assert!(validate_compaction_window(&[ContextItem::CompactionTrigger]).is_err());
    assert!(
        validate_compaction_window(&[ContextItem::LocalCompactionNarrative(
            LocalCompactionNarrativeItem {
                narrative: "private".to_owned(),
            },
        )])
        .is_err()
    );
}

/// Provider message phases retain only the two supported OpenAI wire values.
#[test]
fn message_phases_have_only_provider_supported_wire_values() {
    assert_eq!(MessagePhase::Commentary.as_openai_wire(), "commentary");
    assert_eq!(MessagePhase::FinalAnswer.as_openai_wire(), "final_answer");
}

/// Synthetic compaction-summary origin must survive protocol serialization
/// independently of the exact narrative bytes.
#[test]
fn synthetic_compaction_summary_origin_round_trips() {
    let part = ContentPart::SyntheticCompactionSummary {
        text: "<summary>& exact bytes".to_owned(),
    };
    let encoded = serde_json::to_value(&part).expect("serialize typed origin");
    assert_eq!(
        encoded,
        serde_json::json!({
            "type": "synthetic_compaction_summary",
            "text": "<summary>& exact bytes",
        })
    );
    assert_eq!(
        serde_json::from_value::<ContentPart>(encoded).expect("deserialize typed origin"),
        part
    );
}

/// Streaming raw-CBOR rendering remains byte-identical to the established
/// materialized `ToolResultItem` renderer for every terminal status.
#[test]
fn streaming_provider_tool_result_text_matches_materialized_renderer() {
    let value = CborValue::Map(vec![
        (
            CborValue::Text("status".to_owned()),
            CborValue::Integer(7.into()),
        ),
        (
            CborValue::Text("metadata".to_owned()),
            CborValue::Text("first\nsecond".to_owned()),
        ),
        (
            CborValue::Text("output".to_owned()),
            CborValue::Array(vec![
                CborValue::Text("line\rone".to_owned()),
                CborValue::Map(vec![(
                    CborValue::Text("nested".to_owned()),
                    CborValue::Text("value\n".to_owned()),
                )]),
            ]),
        ),
    ]);
    let cases = [
        (ToolResultStatus::Success, ProviderToolResultStatus::Success),
        (
            ToolResultStatus::Error {
                message: "bad\nheader".to_owned(),
            },
            ProviderToolResultStatus::Error {
                message: "bad\nheader",
            },
        ),
        (
            ToolResultStatus::Cancelled {
                reason: "stop\trequested".to_owned(),
            },
            ProviderToolResultStatus::Cancelled {
                reason: "stop\trequested",
            },
        ),
    ];

    for (status, borrowed_status) in cases {
        let expected = ToolResultItem {
            call_id: ToolCallId::from("call"),
            tool_type: ToolType::Function,
            status,
            output: ToolResponse::from_cbor(&value),
            presentation: crate::ToolResultPresentation::ToolPayload,
            provider_content: Vec::new(),
        }
        .render_provider_text();
        let mut actual = String::new();
        write_provider_tool_result_text(&value, borrowed_status, &mut actual)
            .expect("String sink cannot fail");
        assert_eq!(actual, expected);
        assert_eq!(
            measure_provider_tool_result_text(&value, borrowed_status).rendered_bytes,
            expected.len()
        );
    }
}

/// Shared rendered-key classification keeps materialized and streaming paths
/// identical for scalar, suppressed-data, nested, tagged, and non-text keys.
#[test]
fn provider_tool_result_normalization_matches_across_cbor_shapes() {
    let values = [
        CborValue::Integer(42.into()),
        CborValue::Map(vec![
            (
                CborValue::Text("data".to_owned()),
                CborValue::Text("hidden".to_owned()),
            ),
            (
                CborValue::Text("output".to_owned()),
                CborValue::Text("shown".to_owned()),
            ),
        ]),
        CborValue::Map(vec![
            (
                CborValue::Tag(7, Box::new(CborValue::Text("data".to_owned()))),
                CborValue::Text("hidden-tagged".to_owned()),
            ),
            (
                CborValue::Tag(8, Box::new(CborValue::Text("output".to_owned()))),
                CborValue::Text("shown-tagged".to_owned()),
            ),
        ]),
        CborValue::Map(vec![
            (
                CborValue::Array(vec![CborValue::Text("data".to_owned())]),
                CborValue::Text("hidden-array-key".to_owned()),
            ),
            (
                CborValue::Array(vec![CborValue::Text("line-numbered content".to_owned())]),
                CborValue::Text("shown-array-key".to_owned()),
            ),
        ]),
        CborValue::Map(vec![
            (
                CborValue::Text("meta".to_owned()),
                CborValue::Map(vec![(
                    CborValue::Text("nested".to_owned()),
                    CborValue::Text("one\ntwo".to_owned()),
                )]),
            ),
            (CborValue::Text("plain".to_owned()), CborValue::Bool(true)),
        ]),
    ];

    for value in values {
        let expected = ToolResultItem {
            call_id: ToolCallId::from("call"),
            tool_type: ToolType::Function,
            status: ToolResultStatus::Success,
            output: ToolResponse::from_cbor(&value),
            presentation: crate::ToolResultPresentation::ToolPayload,
            provider_content: Vec::new(),
        }
        .render_provider_text();
        let mut streamed = String::new();
        write_provider_tool_result_text(&value, ProviderToolResultStatus::Success, &mut streamed)
            .expect("String sink cannot fail");

        assert_eq!(streamed, expected);
        assert_eq!(
            measure_provider_tool_result_text(&value, ProviderToolResultStatus::Success)
                .rendered_bytes,
            expected.len()
        );
    }
}

/// Nested-map measurement visits each rendered CBOR node only once rather than
/// recursively rescanning descendants for every ancestor.
#[test]
fn provider_tool_result_measurement_is_linear_for_nested_maps() {
    const DEPTH: usize = 96;
    let mut value = CborValue::Text("leaf".to_owned());
    for depth in 0..DEPTH {
        value = CborValue::Map(vec![(CborValue::Text(format!("level-{depth}")), value)]);
    }

    reset_provider_text_shape_visits();
    let measurement = measure_provider_tool_result_text(&value, ProviderToolResultStatus::Success);
    let visits = provider_text_shape_visits();

    assert!(measurement.rendered_bytes > 0);
    assert!(
        visits <= DEPTH * 2 + 1,
        "expected one visit per key/value node, got {visits}"
    );
}

/// Full rendering caches only emitted structure: arbitrarily many duplicate
/// suppressed keys and one large hidden value do not create per-source-node
/// auxiliary state.
#[test]
fn provider_tool_result_render_cache_omits_suppressed_structure() {
    let mut entries = (0..20_000)
        .map(|_| (CborValue::Text("data".to_owned()), CborValue::Null))
        .collect::<Vec<_>>();
    entries.push((
        CborValue::Text("data".to_owned()),
        CborValue::Array(vec![CborValue::Null; 100_000]),
    ));
    entries.push((
        CborValue::Text("output".to_owned()),
        CborValue::Text("ok".to_owned()),
    ));
    let value = CborValue::Map(entries);

    reset_provider_text_shape_visits();
    let mut rendered = String::new();
    write_provider_tool_result_text(&value, ProviderToolResultStatus::Success, &mut rendered)
        .expect("String sink cannot fail");

    assert_eq!(rendered, "ok");
    assert!(
        provider_text_shape_cache_insertions() <= 2,
        "suppressed source nodes entered the render cache"
    );
}
