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

/// Ensures the closed repair grammar requires one adjacent matching call,
/// bounded nonempty diagnostic, and no unrelated placement.
#[test]
fn v2_dummy_repair_grammar_is_adjacent_and_bounded() {
    let scenario = dummy_repair_scenario("repair");
    assert!(
        FakeConfig {
            scenario: ScenarioConfig::V2(scenario.clone())
        }
        .validate()
        .is_ok()
    );
    for diagnostic in [String::new(), "x".repeat(513)] {
        let mut invalid = scenario.clone();
        let ScenarioActionV2::DummyToolRepair {
            diagnostic: value, ..
        } = &mut invalid.lanes[0].actions[1]
        else {
            unreachable!()
        };
        *value = diagnostic;
        assert!(
            FakeConfig {
                scenario: ScenarioConfig::V2(invalid)
            }
            .validate()
            .is_err()
        );
    }
    let mut mismatch = scenario.clone();
    let ScenarioActionV2::DummyToolRepair { call_id, .. } = &mut mismatch.lanes[0].actions[1]
    else {
        unreachable!()
    };
    *call_id = "wrong".into();
    assert!(
        FakeConfig {
            scenario: ScenarioConfig::V2(mismatch)
        }
        .validate()
        .is_err()
    );
    let mut nonadjacent = scenario;
    nonadjacent.lanes[0].actions.insert(
        1,
        ScenarioActionV2::Text {
            user_text: "between".to_owned(),
            response: "between".to_owned(),
        },
    );
    assert!(
        FakeConfig {
            scenario: ScenarioConfig::V2(nonadjacent)
        }
        .validate()
        .is_err()
    );
}

/// Ensures live repair observations fail closed on wrong calls, duplicates,
/// inversion, and delivery outside the current repair action.
#[test]
fn v2_dummy_repair_live_pair_is_exact_and_ordered() {
    let scenario = dummy_repair_scenario("repair");
    let mut state = FakeState::default();
    state.scenario = Some(ScenarioConfig::V2(scenario));
    state.lane_cursors = vec![1];
    assert!(
        state
            .record_dummy_repair_event(&provider_tool_error("call", "repair"))
            .is_err()
    );
    assert!(state.repair_progress.is_none());
    assert!(
        state
            .record_dummy_repair_event(&tool_error("wrong", "repair"))
            .is_err()
    );
    assert!(state.repair_progress.is_none());
    state
        .record_dummy_repair_event(&tool_error("call", "repair"))
        .expect("exact tool error");
    assert!(
        state
            .record_dummy_repair_event(&tool_error("call", "repair"))
            .is_err()
    );
    state
        .record_dummy_repair_event(&provider_tool_error("call", "repair"))
        .expect("exact provider error");
    state.lane_cursors[0] = 2;
    assert!(
        state
            .record_dummy_repair_event(&tool_error("call", "repair"))
            .is_err()
    );
}

/// Ensures the repair continuation accepts exactly one matching error result
/// and rejects wrong status, diagnostic, call identity, or an extra result.
#[test]
fn v2_dummy_repair_continuation_requires_one_exact_error() {
    let scenario = dummy_repair_scenario("repair");
    let action = scenario.lanes[0].actions[1].clone();
    let agent = tau_proto::AgentId::parse("agent").expect("agent id");
    let mut state = FakeState::default();
    state.scenario = Some(ScenarioConfig::V2(scenario));
    state.lane_cursors = vec![1];
    state.agent_lanes = HashMap::from([(agent.clone(), 0)]);
    let mut prompt = prompt_for(&agent, "continue", None);
    prompt
        .context
        .blocks
        .push(tau_proto::ContextBlock::ToolResults(
            tau_proto::ToolResultsBlock {
                items: vec![error_tool_result("call", "repair")],
            },
        ));
    let exact = prompt.clone();

    latest_tool_results_mut(&mut prompt).items[0].status = tau_proto::ToolResultStatus::Success;
    assert!(state.validate_v2_action(1, &prompt, &action).is_err());
    prompt = exact.clone();
    latest_tool_results_mut(&mut prompt).items[0] = error_tool_result("call", "wrong");
    assert!(state.validate_v2_action(1, &prompt, &action).is_err());
    prompt = exact.clone();
    latest_tool_results_mut(&mut prompt).items[0] = error_tool_result("wrong", "repair");
    assert!(state.validate_v2_action(1, &prompt, &action).is_err());
    prompt = exact.clone();
    latest_tool_results_mut(&mut prompt)
        .items
        .push(error_tool_result("extra", "repair"));
    assert!(state.validate_v2_action(1, &prompt, &action).is_err());
    state
        .validate_and_commit_v2_action(0, 1, &exact, &action)
        .expect("exact repaired result commits");
    assert_eq!(state.lane_cursors, [2]);
}

/// Ensures production `agent_start` remains at most two exact, bounded,
/// adjacent call/result pairs rather than a generic harness-tool grammar.
#[test]
fn v2_agent_start_actions_require_at_most_two_bounded_adjacent_pairs() {
    let pair = agent_start_scenario();
    assert!(
        FakeConfig {
            scenario: ScenarioConfig::V2(pair.clone())
        }
        .validate()
        .is_ok()
    );

    let mut mismatched = pair.clone();
    let ScenarioActionV2::AgentStartResult { call_id, .. } = &mut mismatched.lanes[0].actions[1]
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

    let mut unpaired = pair.clone();
    unpaired.lanes[0].actions.pop();
    assert!(
        FakeConfig {
            scenario: ScenarioConfig::V2(unpaired)
        }
        .validate()
        .is_err()
    );

    let mut two_pairs = pair.clone();
    let mut second_pair = pair.lanes[0].actions.clone();
    for action in &mut second_pair {
        match action {
            ScenarioActionV2::AgentStartCall {
                call_id, user_text, ..
            }
            | ScenarioActionV2::AgentStartResult {
                call_id, user_text, ..
            } => {
                *call_id = "second-call".into();
                *user_text = "start second".to_owned();
            }
            _ => unreachable!(),
        }
    }
    two_pairs.lanes[0].actions.extend(second_pair.clone());
    assert!(
        FakeConfig {
            scenario: ScenarioConfig::V2(two_pairs.clone())
        }
        .validate()
        .is_ok()
    );

    let mut three_pairs = two_pairs;
    for action in &mut second_pair {
        match action {
            ScenarioActionV2::AgentStartCall { call_id, .. }
            | ScenarioActionV2::AgentStartResult { call_id, .. } => {
                *call_id = "third-call".into();
            }
            _ => unreachable!(),
        }
    }
    three_pairs.lanes[0].actions.extend(second_pair);
    assert!(
        FakeConfig {
            scenario: ScenarioConfig::V2(three_pairs)
        }
        .validate()
        .is_err()
    );

    for invalid in [String::new(), "x".repeat(4 * 1024 + 1)] {
        let mut bounded = pair.clone();
        let ScenarioActionV2::AgentStartCall { prompt, .. } = &mut bounded.lanes[0].actions[0]
        else {
            unreachable!()
        };
        *prompt = invalid;
        assert!(
            FakeConfig {
                scenario: ScenarioConfig::V2(bounded)
            }
            .validate()
            .is_err()
        );
    }
}

/// Ensures explicit watch recreation is one exact adjacent call/result pair,
/// rather than a generic dynamic tool grammar.
#[test]
fn v2_agent_watch_actions_require_one_bounded_adjacent_pair() {
    let pair = agent_watch_scenario();
    assert!(
        FakeConfig {
            scenario: ScenarioConfig::V2(pair.clone())
        }
        .validate()
        .is_ok()
    );

    let mut mismatched = pair.clone();
    let ScenarioActionV2::AgentWatchResult { call_id, .. } = &mut mismatched.lanes[0].actions[1]
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

    let mut unpaired = pair.clone();
    unpaired.lanes[0].actions.pop();
    assert!(
        FakeConfig {
            scenario: ScenarioConfig::V2(unpaired)
        }
        .validate()
        .is_err()
    );

    let mut repeated = pair.clone();
    repeated.lanes[0]
        .actions
        .extend(pair.lanes[0].actions.clone());
    assert!(
        FakeConfig {
            scenario: ScenarioConfig::V2(repeated)
        }
        .validate()
        .is_err()
    );

    for call_id in [String::new(), "x".repeat(257)] {
        let mut bounded = pair.clone();
        let ScenarioActionV2::AgentWatchCall {
            call_id: request_id,
            ..
        } = &mut bounded.lanes[0].actions[0]
        else {
            unreachable!()
        };
        *request_id = call_id.clone().into();
        let ScenarioActionV2::AgentWatchResult {
            call_id: result_id, ..
        } = &mut bounded.lanes[0].actions[1]
        else {
            unreachable!()
        };
        *result_id = call_id.into();
        assert!(
            FakeConfig {
                scenario: ScenarioConfig::V2(bounded)
            }
            .validate()
            .is_err()
        );
    }
}

/// Rejects a watch schema or result text that is not exactly correlated to the
/// child learned from `agent_start`, without consuming either action.
#[test]
fn v2_agent_watch_runtime_mismatches_leave_state_unconsumed() {
    let scenario = agent_watch_scenario();
    let parent = tau_proto::AgentId::parse("parent").expect("parent id");
    let child = tau_proto::AgentId::parse("child").expect("child id");
    let call = scenario.lanes[0].actions[0].clone();
    let result = scenario.lanes[0].actions[1].clone();
    let mut state = FakeState::default();
    state.scenario = Some(ScenarioConfig::V2(scenario));
    state.lane_cursors = vec![0];
    state.child_agents = HashMap::from([(parent.clone(), vec![child.clone()])]);

    let mut call_prompt = prompt_for(&parent, "watch", Some("lane"));
    call_prompt.tools = vec![
        tau_proto::ToolDefinition {
            name: ToolName::new("agent_start"),
            model_visible_name: None,
            description: None,
            tool_type: ToolType::Function,
            parameters: Some(agent_start_parameters()),
            format: None,
        },
        tau_proto::ToolDefinition {
            name: ToolName::new("agent_watch"),
            model_visible_name: None,
            description: None,
            tool_type: ToolType::Function,
            parameters: None,
            format: None,
        },
    ];
    assert!(
        state
            .validate_and_commit_v2_action(0, 0, &call_prompt, &call)
            .is_err()
    );
    assert_eq!(state.lane_cursors, [0]);

    call_prompt.tools[1].parameters = Some(agent_watch_parameters());
    call_prompt.tools[0].parameters = None;
    assert!(
        state
            .validate_and_commit_v2_action(0, 0, &call_prompt, &call)
            .is_err()
    );
    assert_eq!(state.lane_cursors, [0]);

    call_prompt.tools[0].parameters = Some(agent_start_parameters());
    state
        .validate_and_commit_v2_action(0, 0, &call_prompt, &call)
        .expect("exact watch tool snapshot commits");
    assert_eq!(state.lane_cursors, [1]);

    let mut result_prompt = prompt_for(&parent, "watch", None);
    result_prompt
        .context
        .blocks
        .push(tau_proto::ContextBlock::ToolResults(
            tau_proto::ToolResultsBlock {
                items: vec![tool_result(
                    "watch-call",
                    "Watching agent `child`; subscription_id=forbidden",
                )],
            },
        ));
    assert!(
        state
            .validate_and_commit_v2_action(0, 1, &result_prompt, &result)
            .is_err()
    );
    assert_eq!(state.lane_cursors, [1]);

    latest_tool_results_mut(&mut result_prompt).items[0] =
        tool_result("wrong-call", "Watching agent `child`");
    assert!(
        state
            .validate_and_commit_v2_action(0, 1, &result_prompt, &result)
            .is_err()
    );
    assert_eq!(state.lane_cursors, [1]);

    latest_tool_results_mut(&mut result_prompt).items[0] =
        tool_result("watch-call", "Watching agent `child`");
    latest_tool_results_mut(&mut result_prompt).items[0].status =
        tau_proto::ToolResultStatus::Error {
            message: "failed".to_owned(),
        };
    assert!(
        state
            .validate_and_commit_v2_action(0, 1, &result_prompt, &result)
            .is_err()
    );
    assert_eq!(state.lane_cursors, [1]);

    latest_tool_results_mut(&mut result_prompt).items[0].status =
        tau_proto::ToolResultStatus::Success;
    latest_tool_results_mut(&mut result_prompt)
        .items
        .push(tool_result("extra", "unexpected current result"));
    assert!(
        state
            .validate_and_commit_v2_action(0, 1, &result_prompt, &result)
            .is_err()
    );
    assert_eq!(state.lane_cursors, [1]);

    latest_tool_results_mut(&mut result_prompt).items.pop();
    state
        .validate_and_commit_v2_action(0, 1, &result_prompt, &result)
        .expect("exact sanitized watch result commits");
    assert_eq!(state.lane_cursors, [2]);
}

/// Requires the S5 watch-result switch to match only the exact sanitized
/// dispatch-uncertain/unknown status text.
#[test]
fn v2_agent_watch_dispatch_uncertain_result_is_exact() {
    let mut scenario = agent_watch_scenario();
    let ScenarioActionV2::AgentWatchResult { expectation, .. } = &mut scenario.lanes[0].actions[1]
    else {
        unreachable!()
    };
    *expectation = AgentWatchResultExpectationV2::DispatchUncertainUnknown;
    let result = scenario.lanes[0].actions[1].clone();
    let parent = tau_proto::AgentId::parse("parent").expect("parent id");
    let child = tau_proto::AgentId::parse("child").expect("child id");
    let mut state = FakeState::default();
    state.scenario = Some(ScenarioConfig::V2(scenario));
    state.lane_cursors = vec![1];
    state.child_agents = HashMap::from([(parent.clone(), vec![child])]);
    state.agent_lanes.insert(parent.clone(), 0);

    let mut prompt = prompt_for(&parent, "watch", None);
    prompt
        .context
        .blocks
        .push(tau_proto::ContextBlock::ToolResults(
            tau_proto::ToolResultsBlock {
                items: vec![tool_result("watch-call", "Watching agent `child`")],
            },
        ));
    assert!(
        state
            .validate_and_commit_v2_action(0, 1, &prompt, &result)
            .is_err()
    );
    assert_eq!(state.lane_cursors, [1]);

    latest_tool_results_mut(&mut prompt).items[0] = tool_result(
        "watch-call",
        "Watching agent `child`; current status: dispatch uncertain (unknown)",
    );
    state
        .validate_and_commit_v2_action(0, 1, &prompt, &result)
        .expect("exact dispatch-uncertain watch result commits");
    assert_eq!(state.lane_cursors, [2]);
}

/// Ensures automatic-watch batches reject empty, oversized, or unbounded
/// content before the provider can subscribe to live traffic.
#[test]
fn v2_watch_notification_actions_are_closed_and_bounded() {
    let action = |notifications| ScenarioActionV2::WatchNotifications {
        notifications,
        response: "complete".to_owned(),
    };
    assert!(
        FakeConfig {
            scenario: ScenarioConfig::V2(v2_action(action(vec![
                WatchNotificationV2::TurnState {
                    state: tau_proto::AgentRuntimeState::Running,
                },
                WatchNotificationV2::Response {
                    content: "done".to_owned(),
                },
                WatchNotificationV2::TurnState {
                    state: tau_proto::AgentRuntimeState::Idle,
                },
            ])))
        }
        .validate()
        .is_ok()
    );
    for notifications in [
        Vec::new(),
        vec![
            WatchNotificationV2::TurnState {
                state: tau_proto::AgentRuntimeState::Running,
            };
            5
        ],
        vec![WatchNotificationV2::Response {
            content: String::new(),
        }],
        vec![WatchNotificationV2::Prompt {
            content: "x".repeat(4 * 1024 + 1),
        }],
    ] {
        assert!(
            FakeConfig {
                scenario: ScenarioConfig::V2(v2_action(action(notifications)))
            }
            .validate()
            .is_err()
        );
    }

    let chains = |prompt, response| {
        v2_action(ScenarioActionV2::WatchNotificationChains {
            prompt,
            response,
            completion: "complete".to_owned(),
        })
    };
    assert!(
        FakeConfig {
            scenario: ScenarioConfig::V2(chains("prompt".to_owned(), "response".to_owned()))
        }
        .validate()
        .is_ok()
    );
    for scenario in [
        chains(String::new(), "response".to_owned()),
        chains("x".repeat(4 * 1024 + 1), "response".to_owned()),
        chains("prompt".to_owned(), String::new()),
        chains("prompt".to_owned(), "x".repeat(4 * 1024 + 1)),
    ] {
        assert!(
            FakeConfig {
                scenario: ScenarioConfig::V2(scenario)
            }
            .validate()
            .is_err()
        );
    }
}

/// Accepts either cross-stream interleaving of the prompt/response and
/// running/idle chains while rejecting each causal inversion before admission.
#[test]
fn v2_watch_notification_chains_enforce_only_their_two_predecessors() {
    let parent = tau_proto::AgentId::parse("parent").expect("parent id");
    let child = tau_proto::AgentId::parse("child").expect("child id");
    let scenario = || {
        v2_action(ScenarioActionV2::WatchNotificationChains {
            prompt: "work".to_owned(),
            response: "done".to_owned(),
            completion: "complete".to_owned(),
        })
    };
    let state = || {
        let mut state = FakeState::default();
        state.scenario = Some(ScenarioConfig::V2(scenario()));
        state.lane_cursors = vec![0];
        state.agent_lanes = HashMap::from([(parent.clone(), 0)]);
        state.child_agents = HashMap::from([(parent.clone(), vec![child.clone()])]);
        state
    };

    let mut alternate = state();
    for message in [
        watch_turn(&parent, &child, tau_proto::AgentRuntimeState::Running, 7),
        watch_prompt(&parent, &child, "work"),
        watch_turn(&parent, &child, tau_proto::AgentRuntimeState::Idle, 7),
        watch_response(&parent, &child, "done"),
    ] {
        alternate
            .record_watch_notification(&message)
            .expect("valid partial-order interleaving");
    }
    assert_eq!(alternate.watch_notifications[&parent].len(), 4);

    let mut response_first = state();
    assert!(
        response_first
            .record_watch_notification(&watch_response(&parent, &child, "done"))
            .is_err()
    );
    assert!(response_first.watch_notifications.is_empty());

    let mut idle_first = state();
    assert!(
        idle_first
            .record_watch_notification(&watch_turn(
                &parent,
                &child,
                tau_proto::AgentRuntimeState::Idle,
                7,
            ))
            .is_err()
    );
    assert!(idle_first.watch_notifications.is_empty());
}

/// Rejects unrelated, malformed, re-correlated, and excess live watch records
/// without advancing the lane cursor or admitting the bad record.
#[test]
fn v2_watch_runtime_mismatches_leave_the_action_unconsumed() {
    let notifications = vec![
        WatchNotificationV2::TurnState {
            state: tau_proto::AgentRuntimeState::Running,
        },
        WatchNotificationV2::Response {
            content: "done".to_owned(),
        },
        WatchNotificationV2::TurnState {
            state: tau_proto::AgentRuntimeState::Idle,
        },
    ];
    let scenario = v2_action(ScenarioActionV2::WatchNotifications {
        notifications,
        response: "complete".to_owned(),
    });
    let parent = tau_proto::AgentId::parse("parent").expect("parent id");
    let child = tau_proto::AgentId::parse("child").expect("child id");
    let mut state = FakeState::default();
    state.scenario = Some(ScenarioConfig::V2(scenario));
    state.lane_cursors = vec![0];
    state.agent_lanes = HashMap::from([(parent.clone(), 0)]);
    state.child_agents = HashMap::from([(parent.clone(), vec![child.clone()])]);

    let mut wrong_sender = watch_turn(
        &parent,
        &tau_proto::AgentId::parse("other").expect("other id"),
        tau_proto::AgentRuntimeState::Running,
        7,
    );
    assert!(state.record_watch_notification(&wrong_sender).is_err());
    assert!(state.watch_notifications.is_empty());

    wrong_sender.sender_id = child.clone();
    state
        .record_watch_notification(&wrong_sender)
        .expect("exact running notification");
    assert_eq!(state.watch_notifications[&parent].len(), 1);

    let mut wrong_content = watch_response(&parent, &child, "other");
    assert!(state.record_watch_notification(&wrong_content).is_err());
    assert_eq!(state.watch_notifications[&parent].len(), 1);
    wrong_content.message = "done".to_owned();
    state
        .record_watch_notification(&wrong_content)
        .expect("exact response notification");

    let wrong_generation = watch_turn(&parent, &child, tau_proto::AgentRuntimeState::Idle, 8);
    assert!(state.record_watch_notification(&wrong_generation).is_err());
    assert_eq!(state.watch_notifications[&parent].len(), 2);
    let idle = watch_turn(&parent, &child, tau_proto::AgentRuntimeState::Idle, 7);
    state
        .record_watch_notification(&idle)
        .expect("exact idle notification");
    assert!(state.record_watch_notification(&idle).is_err());
    assert_eq!(state.watch_notifications[&parent].len(), 3);
    assert_eq!(state.lane_cursors, [0]);
    assert_eq!(state.agent_lanes.get(&parent), Some(&0));
}

/// Allows no-context multi-lane binding only for the exact retained child and
/// only when its first prompt selects one unique unbound lane.
#[test]
fn v2_no_context_lane_binding_requires_the_unique_retained_child() {
    let scenario = ScenarioV2::new(
        "child-binding",
        vec![
            ScenarioLaneV2 {
                ctx_id: "main".to_owned(),
                actions: vec![ScenarioActionV2::Text {
                    user_text: "main".to_owned(),
                    response: "main".to_owned(),
                }],
            },
            ScenarioLaneV2 {
                ctx_id: "worker".to_owned(),
                actions: vec![ScenarioActionV2::Text {
                    user_text: "worker".to_owned(),
                    response: "worker".to_owned(),
                }],
            },
        ],
    );
    let parent = tau_proto::AgentId::parse("parent").expect("parent id");
    let child = tau_proto::AgentId::parse("child").expect("child id");
    let mut state = FakeState::default();
    state.scenario = Some(ScenarioConfig::V2(scenario));
    state.lane_cursors = vec![0, 0];
    state.agent_lanes = HashMap::from([(parent.clone(), 0)]);
    state.child_agents = HashMap::from([(parent, vec![child.clone()])]);
    assert_eq!(
        state
            .select_v2_lane(&prompt_for(&child, "worker", None))
            .expect("unique retained child binds"),
        1
    );
    let other = tau_proto::AgentId::parse("other").expect("other id");
    assert!(
        state
            .select_v2_lane(&prompt_for(&other, "worker", None))
            .is_err()
    );
    assert!(
        state
            .select_v2_lane(&prompt_for(&other, "main", Some("main")))
            .is_err()
    );

    let ScenarioConfig::V2(scenario) = state.scenario.as_mut().expect("scenario") else {
        unreachable!()
    };
    scenario.lanes.push(ScenarioLaneV2 {
        ctx_id: "worker-duplicate".to_owned(),
        actions: vec![ScenarioActionV2::Text {
            user_text: "worker".to_owned(),
            response: "duplicate".to_owned(),
        }],
    });
    state.lane_cursors.push(0);
    assert!(
        state
            .select_v2_lane(&prompt_for(&child, "worker", None))
            .is_err()
    );
    assert!(!state.agent_lanes.contains_key(&child));
    assert_eq!(state.lane_cursors, [0, 0, 0]);

    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let checkpoint = tempdir.path().join("cursor.json");
    let mut sole = FakeState::default();
    sole.scenario = Some(ScenarioConfig::V2(v2_action(ScenarioActionV2::Text {
        user_text: "next".to_owned(),
        response: "next".to_owned(),
    })));
    sole.lane_cursors = vec![0];
    sole.agent_lanes = HashMap::from([(tau_proto::AgentId::parse("first").expect("first id"), 0)]);
    sole.checkpoint = Some(checkpoint.clone());
    assert!(
        sole.select_v2_lane(&prompt_for(&other, "next", None))
            .is_err()
    );
    assert_eq!(sole.lane_cursors, [0]);
    assert_eq!(sole.agent_lanes.len(), 1);
    assert!(!checkpoint.exists());
}

/// Rejects an `agent_start` schema mismatch and unrelated extra tool results
/// before committing the cursor, lane, child association, or checkpoint.
#[test]
fn v2_agent_start_runtime_mismatches_leave_state_unconsumed() {
    let scenario = agent_start_scenario();
    let parent = tau_proto::AgentId::parse("parent").expect("parent id");
    let child = tau_proto::AgentId::parse("child").expect("child id");
    let call = scenario.lanes[0].actions[0].clone();
    let result = scenario.lanes[0].actions[1].clone();
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let checkpoint = tempdir.path().join("cursor.json");
    let mut call_state = FakeState::default();
    call_state.scenario = Some(ScenarioConfig::V2(scenario.clone()));
    call_state.lane_cursors = vec![0];
    call_state.checkpoint = Some(checkpoint.clone());
    let call_prompt = prompt_for(&parent, "start", Some("lane"));
    assert!(
        call_state
            .validate_and_commit_v2_action(0, 0, &call_prompt, &call)
            .is_err()
    );
    assert_eq!(call_state.lane_cursors, [0]);
    assert!(call_state.agent_lanes.is_empty());
    assert!(call_state.child_agents.is_empty());
    assert!(!checkpoint.exists());

    let mut result_state = FakeState::default();
    result_state.scenario = Some(ScenarioConfig::V2(scenario));
    result_state.lane_cursors = vec![1];
    result_state.agent_lanes = HashMap::from([(parent.clone(), 0)]);
    result_state.checkpoint = Some(checkpoint.clone());
    let mut result_prompt = prompt_for(&parent, "start", None);
    result_prompt
        .context
        .blocks
        .push(tau_proto::ContextBlock::ToolResults(
            tau_proto::ToolResultsBlock {
                items: vec![
                    start_result("call", &parent, &child),
                    tau_proto::ToolResultItem {
                        call_id: "unrelated".into(),
                        tool_type: ToolType::Function,
                        status: tau_proto::ToolResultStatus::Success,
                        output: tau_proto::ToolResponse::from_cbor(&CborValue::Text(
                            "unrelated".to_owned(),
                        )),
                        provider_content: Vec::new(),
                    },
                ],
            },
        ));
    assert!(
        result_state
            .validate_and_commit_v2_action(0, 1, &result_prompt, &result)
            .is_err()
    );
    assert_eq!(result_state.lane_cursors, [1]);
    assert!(result_state.child_agents.is_empty());
    assert!(!checkpoint.exists());

    let tau_proto::ContextBlock::ToolResults(results) = result_prompt
        .context
        .blocks
        .last_mut()
        .expect("result block")
    else {
        unreachable!()
    };
    results.items.pop();
    result_state
        .validate_and_commit_v2_action(0, 1, &result_prompt, &result)
        .expect("exact sole result commits");
    assert_eq!(result_state.lane_cursors, [2]);
    assert_eq!(result_state.child_agents[&parent], [child]);
    assert!(checkpoint.exists());
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

fn agent_start_scenario() -> ScenarioV2 {
    ScenarioV2::new(
        "agent-start-validation",
        vec![ScenarioLaneV2 {
            ctx_id: "lane".to_owned(),
            actions: vec![
                ScenarioActionV2::AgentStartCall {
                    user_text: "start".to_owned(),
                    call_id: "call".into(),
                    prompt: "work".to_owned(),
                    role: Some("worker".to_owned()),
                    task_name: "worker task".to_owned(),
                },
                ScenarioActionV2::AgentStartResult {
                    user_text: "start".to_owned(),
                    call_id: "call".into(),
                    response: "started".to_owned(),
                },
            ],
        }],
    )
}

fn two_agent_start_scenario() -> ScenarioV2 {
    let mut scenario = agent_start_scenario();
    let mut second = scenario.lanes[0].actions.clone();
    for action in &mut second {
        match action {
            ScenarioActionV2::AgentStartCall {
                call_id, user_text, ..
            }
            | ScenarioActionV2::AgentStartResult {
                call_id, user_text, ..
            } => {
                *call_id = "second-call".into();
                *user_text = "start second".to_owned();
            }
            _ => unreachable!(),
        }
    }
    scenario.lanes[0].actions.extend(second);
    scenario
}

fn agent_watch_scenario() -> ScenarioV2 {
    ScenarioV2::new(
        "agent-watch-validation",
        vec![ScenarioLaneV2 {
            ctx_id: "lane".to_owned(),
            actions: vec![
                ScenarioActionV2::AgentWatchCall {
                    user_text: "watch".to_owned(),
                    call_id: "watch-call".into(),
                },
                ScenarioActionV2::AgentWatchResult {
                    user_text: "watch".to_owned(),
                    call_id: "watch-call".into(),
                    expectation: AgentWatchResultExpectationV2::Enabled,
                    response: "watching".to_owned(),
                },
            ],
        }],
    )
}

fn watch_response(
    parent: &tau_proto::AgentId,
    child: &tau_proto::AgentId,
    content: &str,
) -> AgentMessageReceived {
    AgentMessageReceived {
        message_id: "watch-response".into(),
        sender_id: child.clone(),
        sender_session_id: None,
        recipient_id: parent.clone(),
        kind: tau_proto::AgentMessageKind::WatchResponse,
        watch_turn_state: None,
        watch_provider_status: None,
        message: content.to_owned(),
    }
}

fn watch_prompt(
    parent: &tau_proto::AgentId,
    child: &tau_proto::AgentId,
    content: &str,
) -> AgentMessageReceived {
    AgentMessageReceived {
        message_id: "watch-prompt".into(),
        sender_id: child.clone(),
        sender_session_id: None,
        recipient_id: parent.clone(),
        kind: tau_proto::AgentMessageKind::WatchPrompt,
        watch_turn_state: None,
        watch_provider_status: None,
        message: content.to_owned(),
    }
}

fn watch_turn(
    parent: &tau_proto::AgentId,
    child: &tau_proto::AgentId,
    state: tau_proto::AgentRuntimeState,
    turn_generation: u64,
) -> AgentMessageReceived {
    AgentMessageReceived {
        message_id: format!("watch-turn-{turn_generation}").into(),
        sender_id: child.clone(),
        sender_session_id: None,
        recipient_id: parent.clone(),
        kind: tau_proto::AgentMessageKind::WatchTurnState,
        watch_turn_state: Some(tau_proto::AgentWatchTurnStateNotification {
            session_id: "session".into(),
            subscription_id: "subscription".to_owned(),
            state,
            initial: false,
            turn_generation,
        }),
        watch_provider_status: None,
        message: String::new(),
    }
}

fn prompt_for(
    agent_id: &tau_proto::AgentId,
    user_text: &str,
    ctx_id: Option<&str>,
) -> tau_proto::AgentPromptCreated {
    tau_proto::AgentPromptCreated {
        agent_prompt_id: "ap-test".into(),
        agent_id: agent_id.clone(),
        session_id: "session".into(),
        system_prompt: String::new(),
        context: tau_proto::PromptContext {
            blocks: vec![tau_proto::ContextBlock::UserInput(
                tau_proto::UserInputBlock {
                    items: vec![ContextItem::Message(MessageItem {
                        role: ContextRole::User,
                        content: vec![ContentPart::Text {
                            text: user_text.to_owned(),
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
        ctx_id: ctx_id.map(ToOwned::to_owned),
        compaction: None,
        operation: tau_proto::PromptOperation::Inference,
    }
}

fn start_result(
    call_id: &str,
    parent: &tau_proto::AgentId,
    child: &tau_proto::AgentId,
) -> tau_proto::ToolResultItem {
    let raw = CborValue::Map(vec![
        (
            CborValue::Text("self_agent_id".to_owned()),
            CborValue::Text(parent.to_string()),
        ),
        (
            CborValue::Text("sub_agent_id".to_owned()),
            CborValue::Text(child.to_string()),
        ),
    ]);
    tau_proto::ToolResultItem {
        call_id: call_id.into(),
        tool_type: ToolType::Function,
        status: tau_proto::ToolResultStatus::Success,
        output: tau_proto::ToolResponse::from_cbor(&raw),
        provider_content: Vec::new(),
    }
}

fn tool_result(call_id: &str, text: &str) -> tau_proto::ToolResultItem {
    tau_proto::ToolResultItem {
        call_id: call_id.into(),
        tool_type: ToolType::Function,
        status: tau_proto::ToolResultStatus::Success,
        output: tau_proto::ToolResponse::from_cbor(&CborValue::Text(text.to_owned())),
        provider_content: Vec::new(),
    }
}

fn error_tool_result(call_id: &str, diagnostic: &str) -> tau_proto::ToolResultItem {
    let mut result = tool_result(call_id, diagnostic);
    result.status = tau_proto::ToolResultStatus::Error {
        message: diagnostic.to_owned(),
    };
    result
}

fn tool_error(call_id: &str, diagnostic: &str) -> Event {
    Event::ToolError(tau_proto::ToolError {
        call_id: call_id.into(),
        tool_name: ToolName::new(tau_ext_test_dummy::RESTART_TEST_DUMMY_TOOL_NAME),
        tool_type: ToolType::Function,
        message: diagnostic.to_owned(),
        details: None,
        originator: tau_proto::PromptOriginator::User,
        display: None,
    })
}

fn provider_tool_error(call_id: &str, diagnostic: &str) -> Event {
    let Event::ToolError(error) = tool_error(call_id, diagnostic) else {
        unreachable!()
    };
    Event::ProviderToolError(error)
}

fn dummy_repair_scenario(diagnostic: &str) -> ScenarioV2 {
    ScenarioV2::new(
        "dummy-repair",
        vec![ScenarioLaneV2 {
            ctx_id: "lane".to_owned(),
            actions: vec![
                ScenarioActionV2::DummyToolCall {
                    user_text: "before".to_owned(),
                    call_id: "call".into(),
                },
                ScenarioActionV2::DummyToolRepair {
                    user_text: "continue".to_owned(),
                    call_id: "call".into(),
                    diagnostic: diagnostic.to_owned(),
                    response: "complete".to_owned(),
                },
            ],
        }],
    )
}

fn latest_tool_results_mut(
    prompt: &mut tau_proto::AgentPromptCreated,
) -> &mut tau_proto::ToolResultsBlock {
    let tau_proto::ContextBlock::ToolResults(results) =
        prompt.context.blocks.last_mut().expect("result block")
    else {
        panic!("latest context block must contain tool results")
    };
    results
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
            child_agents: Vec::new(),
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
            child_agents: Vec::new(),
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
                child_agents: Vec::new(),
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
            child_agents: Vec::new(),
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

/// Rejects oversized or relationally invalid child checkpoints while accepting
/// the one parent/child association produced by the closed start pair.
#[test]
fn v2_checkpoint_bounds_and_correlates_child_bindings() {
    let scenario = agent_start_scenario();
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let checkpoint_path = tempdir.path().join("cursor.json");
    let parent = tau_proto::AgentId::parse("parent").expect("parent id");
    let child = tau_proto::AgentId::parse("child").expect("child id");
    let base = || CursorCheckpoint {
        scenario: scenario.clone(),
        cursors: vec![2],
        agent_lanes: vec![AgentLaneCheckpoint {
            agent_id: parent.clone(),
            lane_index: 0,
        }],
        child_agents: vec![ChildAgentCheckpoint {
            parent_agent_id: parent.clone(),
            start_ordinal: 0,
            child_agent_id: child.clone(),
        }],
    };
    let write = |checkpoint: &CursorCheckpoint| {
        std::fs::write(
            &checkpoint_path,
            serde_json::to_vec(checkpoint).expect("checkpoint serializes"),
        )
        .expect("write checkpoint");
    };

    write(&base());
    let restored = ScenarioConfig::V2(scenario.clone())
        .restore_state(Some(&checkpoint_path))
        .expect("valid child checkpoint restores");
    assert_eq!(
        restored.child_agents[&parent].as_slice(),
        std::slice::from_ref(&child)
    );

    let mut missing_child = base();
    missing_child.child_agents.clear();
    write(&missing_child);
    assert!(
        ScenarioConfig::V2(scenario.clone())
            .restore_state(Some(&checkpoint_path))
            .is_err()
    );

    let mut self_link = base();
    self_link.child_agents[0].child_agent_id = parent.clone();
    write(&self_link);
    assert!(
        ScenarioConfig::V2(scenario.clone())
            .restore_state(Some(&checkpoint_path))
            .is_err()
    );

    let mut missing_parent = base();
    missing_parent.agent_lanes.clear();
    write(&missing_parent);
    assert!(
        ScenarioConfig::V2(scenario.clone())
            .restore_state(Some(&checkpoint_path))
            .is_err()
    );

    let mut unconsumed = base();
    unconsumed.cursors[0] = 1;
    write(&unconsumed);
    assert!(
        ScenarioConfig::V2(scenario.clone())
            .restore_state(Some(&checkpoint_path))
            .is_err()
    );

    let mut repeated = base();
    repeated.child_agents.push(ChildAgentCheckpoint {
        parent_agent_id: tau_proto::AgentId::parse("other-parent").expect("other parent id"),
        start_ordinal: 0,
        child_agent_id: child,
    });
    write(&repeated);
    assert!(
        ScenarioConfig::V2(scenario.clone())
            .restore_state(Some(&checkpoint_path))
            .is_err()
    );

    std::fs::write(
        &checkpoint_path,
        vec![b'x'; MAX_CHECKPOINT_BYTES as usize + 1],
    )
    .expect("write oversized checkpoint");
    assert!(
        ScenarioConfig::V2(scenario)
            .restore_state(Some(&checkpoint_path))
            .is_err()
    );
}

/// Restores both ordered child identities for the bounded two-start grammar and
/// rejects missing or duplicate ordinals.
#[test]
fn v2_checkpoint_restores_two_ordered_children_for_one_parent() {
    let scenario = two_agent_start_scenario();
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let checkpoint_path = tempdir.path().join("cursor.json");
    let parent = tau_proto::AgentId::parse("parent").expect("parent id");
    let first = tau_proto::AgentId::parse("first-child").expect("first child id");
    let second = tau_proto::AgentId::parse("second-child").expect("second child id");
    let checkpoint = CursorCheckpoint {
        scenario: scenario.clone(),
        cursors: vec![4],
        agent_lanes: vec![AgentLaneCheckpoint {
            agent_id: parent.clone(),
            lane_index: 0,
        }],
        child_agents: vec![
            ChildAgentCheckpoint {
                parent_agent_id: parent.clone(),
                start_ordinal: 0,
                child_agent_id: first.clone(),
            },
            ChildAgentCheckpoint {
                parent_agent_id: parent.clone(),
                start_ordinal: 1,
                child_agent_id: second.clone(),
            },
        ],
    };
    let write = |checkpoint: &CursorCheckpoint| {
        std::fs::write(
            &checkpoint_path,
            serde_json::to_vec(checkpoint).expect("checkpoint serializes"),
        )
        .expect("write checkpoint");
    };

    write(&checkpoint);
    let restored = ScenarioConfig::V2(scenario.clone())
        .restore_state(Some(&checkpoint_path))
        .expect("two children restore");
    assert_eq!(restored.child_agents[&parent], [first, second]);

    let mut duplicate_ordinal = checkpoint;
    duplicate_ordinal.child_agents[1].start_ordinal = 0;
    write(&duplicate_ordinal);
    assert!(
        ScenarioConfig::V2(scenario)
            .restore_state(Some(&checkpoint_path))
            .is_err()
    );
}
