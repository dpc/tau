use super::*;
use crate::ScenarioLaneV2;

/// Ensures strict Configure decoding rejects undeclared control fields.
#[test]
fn config_rejects_unknown_fields() {
    let value = serde_json::json!({
        "scenario": ScenarioV1::text_v1("prompt", "response"),
        "command": "escape"
    });
    assert!(serde_json::from_value::<FakeConfig>(value).is_err());
}

/// Ensures phase one accepts exactly its text and single-tool-round grammars.
#[test]
fn validation_accepts_named_scenarios_only() {
    for scenario in [
        ScenarioV1::text_v1("prompt", "response"),
        ScenarioV1::dummy_tool_round_v1("prompt"),
    ] {
        assert!(
            FakeConfig {
                scenario: ScenarioConfig::V1(scenario)
            }
            .validate()
            .is_ok()
        );
    }
    let mut invalid = ScenarioV1::text_v1("prompt", "response");
    invalid.turns.push(ScenarioTurnV1::Text {
        user_text: "extra".to_owned(),
        deltas: vec!["extra".to_owned()],
        response: "extra".to_owned(),
    });
    assert!(
        FakeConfig {
            scenario: ScenarioConfig::V1(invalid)
        }
        .validate()
        .is_err()
    );
}

/// Ensures delta amplification and inconsistent final text fail at Configure.
#[test]
fn validation_bounds_and_matches_deltas() {
    let mut too_many = ScenarioV1::text_v1("prompt", "response");
    let ScenarioTurnV1::Text {
        user_text: _,
        deltas,
        response: _,
    } = &mut too_many.turns[0]
    else {
        unreachable!();
    };
    *deltas = vec![String::new(); MAX_DELTAS + 1];
    assert!(
        FakeConfig {
            scenario: ScenarioConfig::V1(too_many)
        }
        .validate()
        .is_err()
    );

    let mut mismatch = ScenarioV1::text_v1("prompt", "response");
    let ScenarioTurnV1::Text {
        user_text: _,
        deltas,
        response: _,
    } = &mut mismatch.turns[0]
    else {
        unreachable!();
    };
    *deltas = vec!["different".to_owned()];
    assert!(
        FakeConfig {
            scenario: ScenarioConfig::V1(mismatch)
        }
        .validate()
        .is_err()
    );
}

/// Ensures durable V2 accepts only one adjacent, exactly correlated
/// `restart_test_dummy` call/result pair rather than arbitrary tools.
#[test]
fn v2_dummy_tool_actions_require_an_adjacent_matching_pair() {
    let pair = ScenarioV2::new(
        "dummy-pair",
        vec![ScenarioLaneV2 {
            ctx_id: "lane".to_owned(),
            actions: vec![
                ScenarioActionV2::DummyToolCall {
                    user_text: "before".to_owned(),
                    call_id: "call".into(),
                },
                ScenarioActionV2::DummyToolResult {
                    user_text: "before".to_owned(),
                    call_id: "call".into(),
                    response: "complete".to_owned(),
                },
            ],
        }],
    );
    assert!(
        FakeConfig {
            scenario: ScenarioConfig::V2(pair.clone())
        }
        .validate()
        .is_ok()
    );

    let mut mismatched = pair.clone();
    let ScenarioActionV2::DummyToolResult { call_id, .. } = &mut mismatched.lanes[0].actions[1]
    else {
        unreachable!()
    };
    *call_id = "other".into();
    assert!(
        FakeConfig {
            scenario: ScenarioConfig::V2(mismatched)
        }
        .validate()
        .is_err()
    );

    let mut unpaired = pair;
    unpaired.lanes[0].actions.pop();
    assert!(
        FakeConfig {
            scenario: ScenarioConfig::V2(unpaired)
        }
        .validate()
        .is_err()
    );

    let mut repeated = ScenarioV2::new(
        "repeated-dummy-pair",
        vec![ScenarioLaneV2 {
            ctx_id: "lane".to_owned(),
            actions: Vec::new(),
        }],
    );
    for call_id in ["first", "second"] {
        repeated.lanes[0]
            .actions
            .push(ScenarioActionV2::DummyToolCall {
                user_text: "before".to_owned(),
                call_id: call_id.into(),
            });
        repeated.lanes[0]
            .actions
            .push(ScenarioActionV2::DummyToolResult {
                user_text: "before".to_owned(),
                call_id: call_id.into(),
                response: "complete".to_owned(),
            });
    }
    assert!(
        FakeConfig {
            scenario: ScenarioConfig::V2(repeated.clone())
        }
        .validate()
        .is_err()
    );
    let ScenarioActionV2::DummyToolCall { call_id, .. } = &mut repeated.lanes[0].actions[2] else {
        unreachable!()
    };
    *call_id = "first".into();
    let ScenarioActionV2::DummyToolResult { call_id, .. } = &mut repeated.lanes[0].actions[3]
    else {
        unreachable!()
    };
    *call_id = "first".into();
    assert!(
        FakeConfig {
            scenario: ScenarioConfig::V2(repeated)
        }
        .validate()
        .is_err()
    );
}

/// Ensures a dummy schema mismatch is rejected before any durable cursor or
/// agent-lane binding can be advanced.
#[test]
fn v2_dummy_mismatch_leaves_cursor_and_binding_unconsumed() {
    let action = ScenarioActionV2::DummyToolCall {
        user_text: "before".to_owned(),
        call_id: "call".into(),
    };
    let scenario = v2_action(action.clone());
    let mut state = FakeState::default();
    state.scenario = Some(ScenarioConfig::V2(scenario));
    state.lane_cursors = vec![0];
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let checkpoint = tempdir.path().join("cursor.json");
    state.checkpoint = Some(checkpoint.clone());
    let mut prompt = tau_proto::AgentPromptCreated {
        agent_prompt_id: "ap-test-0".into(),
        agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
        session_id: "session".into(),
        system_prompt: String::new(),
        context: tau_proto::PromptContext {
            blocks: vec![tau_proto::ContextBlock::UserInput(
                tau_proto::UserInputBlock {
                    items: vec![ContextItem::Message(MessageItem {
                        role: ContextRole::User,
                        content: vec![ContentPart::Text {
                            text: "before".to_owned(),
                        }],
                        phase: None,
                        responses_raw_json: None,
                    })],
                },
            )],
        },
        tools: Vec::new(),
        tools_ref: None,
        model: FAKE_MODEL_ID.into(),
        model_params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        originator: tau_proto::PromptOriginator::User,
        share_user_cache_key: false,
        ctx_id: Some("lane".to_owned()),
        compaction: None,
        operation: tau_proto::PromptOperation::Inference,
    };
    assert!(
        state
            .validate_and_commit_v2_action(0, 0, &prompt, &action)
            .is_err()
    );
    assert_eq!(state.lane_cursors, [0]);
    assert!(state.agent_lanes.is_empty());
    assert!(!checkpoint.exists());

    prompt.tools = vec![tau_proto::ToolDefinition {
        name: ToolName::new(tau_ext_test_dummy::RESTART_TEST_DUMMY_TOOL_NAME),
        model_visible_name: None,
        description: None,
        tool_type: ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        })),
        format: None,
    }];
    state
        .validate_and_commit_v2_action(0, 0, &prompt, &action)
        .expect("corrected prompt consumes once");
    assert_eq!(state.lane_cursors, [1]);
    assert_eq!(state.agent_lanes.get(&prompt.agent_id), Some(&0));
    let bytes = std::fs::read(&checkpoint).expect("checkpoint committed");
    let saved: CursorCheckpoint = serde_json::from_slice(&bytes).expect("checkpoint decodes");
    assert_eq!(saved.cursors, [1]);
    assert_eq!(saved.agent_lanes.len(), 1);
}

/// Ensures serialized scenario bytes and tool-call identity bounds fail closed.
#[test]
fn validation_bounds_scenario_bytes_and_call_ids() {
    let oversized = ScenarioV1::text_v1("prompt", "x".repeat(MAX_SCENARIO_BYTES));
    assert!(
        FakeConfig {
            scenario: ScenarioConfig::V1(oversized)
        }
        .validate()
        .is_err()
    );

    let empty = ScenarioV1::text_v1("prompt", "");
    let fixed_bytes = serde_json::to_vec(&empty)
        .expect("typed scenario serializes")
        .len();
    let payload_bytes = (MAX_SCENARIO_BYTES - fixed_bytes) / 2;
    let mut near = ScenarioV1::text_v1("prompt", "x".repeat(payload_bytes));
    let ScenarioTurnV1::Text {
        user_text: _,
        deltas,
        response,
    } = &mut near.turns[0]
    else {
        unreachable!();
    };
    *deltas = vec![response.clone()];
    assert!(
        FakeConfig {
            scenario: ScenarioConfig::V1(near)
        }
        .validate()
        .is_ok()
    );

    for (call_id, result_id) in [
        ("".into(), "".into()),
        ("x".repeat(257).into(), "x".repeat(257).into()),
        ("call".into(), "different".into()),
    ] {
        let mut scenario = ScenarioV1::dummy_tool_round_v1("prompt");
        let ScenarioTurnV1::ToolCall {
            user_text: _,
            tool_name: _,
            call_id: actual_call_id,
        } = &mut scenario.turns[0]
        else {
            unreachable!();
        };
        *actual_call_id = call_id;
        let ScenarioTurnV1::ToolResult {
            call_id: actual_result_id,
            response: _,
        } = &mut scenario.turns[1]
        else {
            unreachable!();
        };
        *actual_result_id = result_id;
        assert!(
            FakeConfig {
                scenario: ScenarioConfig::V1(scenario)
            }
            .validate()
            .is_err()
        );
    }
}

/// Ensures diagnostics remain byte-bounded without cutting UTF-8 code points.
#[test]
fn trace_bound_is_utf8_safe() {
    let message = format!("{}é", "x".repeat(1023));
    let bounded = bounded_trace_message(&message);
    assert!(bounded.len() <= 1024);
    assert_eq!(bounded, "x".repeat(1023));
}

fn v2_action(action: ScenarioActionV2) -> ScenarioV2 {
    ScenarioV2::new(
        "v2-validation",
        vec![crate::ScenarioLaneV2 {
            ctx_id: "lane".to_owned(),
            actions: vec![action],
        }],
    )
}

/// Rejects out-of-range hold deadlines and ambiguous lane correlation ids.
#[test]
fn v2_validation_bounds_holds_and_lane_identity() {
    for timeout_ms in [99, 10_001] {
        let scenario = v2_action(ScenarioActionV2::HoldUntilCancel {
            user_text: "hold".to_owned(),
            timeout_ms,
        });
        assert!(
            FakeConfig {
                scenario: ScenarioConfig::V2(scenario)
            }
            .validate()
            .is_err()
        );
    }
    let mut duplicate = v2_action(ScenarioActionV2::Text {
        user_text: "one".to_owned(),
        response: "one".to_owned(),
    });
    duplicate.lanes.push(duplicate.lanes[0].clone());
    assert!(
        FakeConfig {
            scenario: ScenarioConfig::V2(duplicate)
        }
        .validate()
        .is_err()
    );
}

/// Rejects incomplete or inconsistent barriers and accepts a complete pair.
#[test]
fn v2_validation_requires_complete_consistent_barriers() {
    let mut scenario = ScenarioV2::new(
        "bad-barrier",
        vec![
            crate::ScenarioLaneV2 {
                ctx_id: "a".to_owned(),
                actions: vec![ScenarioActionV2::BarrierText {
                    user_text: "a".to_owned(),
                    barrier: "both".to_owned(),
                    participants: 2,
                    response: "a".to_owned(),
                }],
            },
            crate::ScenarioLaneV2 {
                ctx_id: "b".to_owned(),
                actions: vec![ScenarioActionV2::Text {
                    user_text: "b".to_owned(),
                    response: "b".to_owned(),
                }],
            },
        ],
    );
    assert!(
        FakeConfig {
            scenario: ScenarioConfig::V2(scenario.clone())
        }
        .validate()
        .is_err()
    );
    scenario.lanes[1].actions[0] = ScenarioActionV2::BarrierText {
        user_text: "b".to_owned(),
        barrier: "both".to_owned(),
        participants: 2,
        response: "b".to_owned(),
    };
    assert!(
        FakeConfig {
            scenario: ScenarioConfig::V2(scenario)
        }
        .validate()
        .is_ok()
    );

    let mut nonsole = v2_action(ScenarioActionV2::BarrierText {
        user_text: "a".to_owned(),
        barrier: "both".to_owned(),
        participants: 2,
        response: "a".to_owned(),
    });
    nonsole.lanes[0].actions.push(ScenarioActionV2::Text {
        user_text: "later".to_owned(),
        response: "later".to_owned(),
    });
    nonsole.lanes.push(crate::ScenarioLaneV2 {
        ctx_id: "b".to_owned(),
        actions: vec![ScenarioActionV2::BarrierText {
            user_text: "b".to_owned(),
            barrier: "both".to_owned(),
            participants: 2,
            response: "b".to_owned(),
        }],
    });
    assert!(
        FakeConfig {
            scenario: ScenarioConfig::V2(nonsole)
        }
        .validate()
        .is_err()
    );

    let inconsistent = ScenarioV2::new(
        "inconsistent-barrier",
        vec![
            crate::ScenarioLaneV2 {
                ctx_id: "a".to_owned(),
                actions: vec![ScenarioActionV2::BarrierText {
                    user_text: "a".to_owned(),
                    barrier: "shared".to_owned(),
                    participants: 2,
                    response: "a".to_owned(),
                }],
            },
            crate::ScenarioLaneV2 {
                ctx_id: "b".to_owned(),
                actions: vec![ScenarioActionV2::BarrierText {
                    user_text: "b".to_owned(),
                    barrier: "shared".to_owned(),
                    participants: 3,
                    response: "b".to_owned(),
                }],
            },
            crate::ScenarioLaneV2 {
                ctx_id: "c".to_owned(),
                actions: vec![ScenarioActionV2::Text {
                    user_text: "c".to_owned(),
                    response: "c".to_owned(),
                }],
            },
        ],
    );
    assert!(
        FakeConfig {
            scenario: ScenarioConfig::V2(inconsistent)
        }
        .validate()
        .is_err()
    );
}

/// Rejects checkpoints whose scenario identity or durable binding is invalid.
#[test]
fn v2_checkpoint_rejects_changed_scenario_and_invalid_bindings() {
    let scenario = v2_action(ScenarioActionV2::Text {
        user_text: "one".to_owned(),
        response: "one".to_owned(),
    });
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let checkpoint_path = tempdir.path().join("cursor.json");
    let changed = v2_action(ScenarioActionV2::Text {
        user_text: "different".to_owned(),
        response: "different".to_owned(),
    });
    std::fs::write(
        &checkpoint_path,
        serde_json::to_vec(&CursorCheckpoint {
            scenario: changed,
            cursors: vec![0],
            agent_lanes: Vec::new(),
        })
        .expect("checkpoint serializes"),
    )
    .expect("write checkpoint");
    assert!(
        ScenarioConfig::V2(scenario.clone())
            .restore_state(Some(&checkpoint_path))
            .is_err()
    );

    std::fs::write(
        &checkpoint_path,
        serde_json::to_vec(&CursorCheckpoint {
            scenario: scenario.clone(),
            cursors: vec![0],
            agent_lanes: vec![AgentLaneCheckpoint {
                agent_id: tau_proto::AgentId::parse("agent").expect("valid agent id"),
                lane_index: 1,
            }],
        })
        .expect("checkpoint serializes"),
    )
    .expect("write checkpoint");
    assert!(
        ScenarioConfig::V2(scenario)
            .restore_state(Some(&checkpoint_path))
            .is_err()
    );

    std::fs::write(&checkpoint_path, b"{not-json").expect("write malformed checkpoint");
    let scenario = v2_action(ScenarioActionV2::Text {
        user_text: "one".to_owned(),
        response: "one".to_owned(),
    });
    assert!(
        ScenarioConfig::V2(scenario.clone())
            .restore_state(Some(&checkpoint_path))
            .is_err()
    );

    for cursors in [Vec::new(), vec![2]] {
        std::fs::write(
            &checkpoint_path,
            serde_json::to_vec(&CursorCheckpoint {
                scenario: scenario.clone(),
                cursors,
                agent_lanes: Vec::new(),
            })
            .expect("checkpoint serializes"),
        )
        .expect("write checkpoint");
        assert!(
            ScenarioConfig::V2(scenario.clone())
                .restore_state(Some(&checkpoint_path))
                .is_err()
        );
    }

    let duplicate_agent = tau_proto::AgentId::parse("agent").expect("valid agent id");
    std::fs::write(
        &checkpoint_path,
        serde_json::to_vec(&CursorCheckpoint {
            scenario: scenario.clone(),
            cursors: vec![0],
            agent_lanes: vec![
                AgentLaneCheckpoint {
                    agent_id: duplicate_agent.clone(),
                    lane_index: 0,
                },
                AgentLaneCheckpoint {
                    agent_id: duplicate_agent,
                    lane_index: 0,
                },
            ],
        })
        .expect("checkpoint serializes"),
    )
    .expect("write checkpoint");
    assert!(
        ScenarioConfig::V2(scenario)
            .restore_state(Some(&checkpoint_path))
            .is_err()
    );
}
