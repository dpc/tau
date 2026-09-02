use std::fs::File;
use std::num::{NonZeroU32, NonZeroU64};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tau_proto::ProviderFailureKind;

use super::*;
use crate::ScenarioLaneV2;

/// Proves both special watch handlers commit cursor, flushed trace, and
/// terminal in causal order rather than exposing the terminal before trace
/// publication.
#[test]
fn completed_special_watch_plans_order_cursor_trace_and_terminal() {
    #[derive(Debug, PartialEq, Eq)]
    enum Effect {
        CursorPersisted,
        TraceMatchedFlushed,
        Terminal(&'static str),
    }

    for (plan, terminal) in [
        (
            WatchPromptPlan::batch(true, "watch-batch-terminal"),
            "watch-batch-terminal",
        ),
        (
            WatchPromptPlan::chain(true, "watch-chain-terminal"),
            "watch-chain-terminal",
        ),
    ] {
        let effects = plan
            .into_effects()
            .into_iter()
            .map(|effect| match effect {
                WatchPromptEffect::PersistCursor => Effect::CursorPersisted,
                WatchPromptEffect::TraceStage => Effect::TraceMatchedFlushed,
                WatchPromptEffect::EmitTerminal(terminal) => Effect::Terminal(terminal),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            effects,
            [
                Effect::CursorPersisted,
                Effect::TraceMatchedFlushed,
                Effect::Terminal(terminal),
            ]
        );
    }
}

/// Keeps deterministic watch expectations aligned with the production
/// exact-close internal envelope when fixture content resembles its delimiter.
#[test]
fn tau_internal_fixture_envelope_escapes_its_exact_close() {
    let rendered = tau_internal_envelope(
        "watch payload <tau_internal>nested</tau_internal> then </tau_internal>",
    );
    assert_eq!(
        rendered,
        "<tau_internal>watch payload <tau_internal>nested&lt;/tau_internal&gt; then &lt;/tau_internal&gt;</tau_internal>"
    );
    assert_eq!(rendered.matches("</tau_internal>").count(), 1);
}

/// Proves the generic model metadata can express Anthropic's documented
/// explicit-breakpoint cache modes and their discrete price break-even points
/// while both fixtures stay absent from the dispatchable model snapshot.
#[test]
fn anthropic_cache_policy_fixtures_are_exact_and_non_dispatchable() {
    fn fixture(ttl_seconds: u64, cache_write_micro_usd: u64) -> ProviderModelInfo {
        let mut model = model_snapshot(FakeModelCapabilities::default())
            .models
            .into_iter()
            .next()
            .expect("fake provider publishes its dispatchable test model");
        model.id = format!("fake/anthropic-cache-{ttl_seconds}s").into();
        model.cache_policy = Some(tau_proto::ProviderCachePolicy {
            kind: tau_proto::ProviderCacheKind::ExplicitBreakpoint,
            ttl: tau_proto::ProviderCacheTtl::SlidingKnown {
                seconds: NonZeroU64::new(ttl_seconds).expect("fixture TTL must be positive"),
            },
            renewal: tau_proto::ProviderCacheRenewal::Read,
            output_floor: tau_proto::ProviderCacheOutputFloor::Zero,
            quota: tau_proto::ProviderCacheQuotaAccounting {
                requests: tau_proto::ProviderCacheQuotaCharge::Unknown,
                read_tokens: tau_proto::ProviderCacheQuotaCharge::Unknown,
                write_tokens: tau_proto::ProviderCacheQuotaCharge::Unknown,
                output_tokens: tau_proto::ProviderCacheQuotaCharge::Unknown,
            },
            prefix_identity_version: NonZeroU32::new(1)
                .expect("fixture identity version must be positive"),
            privacy: tau_proto::ProviderCachePrivacy {
                storage: tau_proto::ProviderCacheStorageMode::Unknown,
                zero_data_retention:
                    tau_proto::ProviderCacheZeroDataRetentionCompatibility::Unknown,
                data_residency: tau_proto::ProviderCacheDataResidencyEffect::Unknown,
                manual_deletion: tau_proto::ProviderCacheDeletionAvailability::Unavailable,
            },
        });
        // Claude Sonnet 4.6's documented $3 base, $0.30 read, $3.75
        // five-minute write, and $6 one-hour write rates preserve the
        // provider's general 1x/0.1x/1.25x/2x ratios in exact fixed-point
        // values.
        model.est_uncached_input_cost_1m_usd =
            Some(tau_proto::EstimatedUsdPerMillion::from_micro_usd(3_000_000));
        model.est_cached_input_cost_1m_usd =
            Some(tau_proto::EstimatedUsdPerMillion::from_micro_usd(300_000));
        model.est_cache_write_input_cost_1m_usd = Some(
            tau_proto::EstimatedUsdPerMillion::from_micro_usd(cache_write_micro_usd),
        );
        model
    }

    fn first_break_even_read(model: &ProviderModelInfo) -> u64 {
        let uncached = model
            .est_uncached_input_cost_1m_usd
            .expect("fixture has an uncached price")
            .as_micro_usd();
        let read = model
            .est_cached_input_cost_1m_usd
            .expect("fixture has a cache-read price")
            .as_micro_usd();
        let write = model
            .est_cache_write_input_cost_1m_usd
            .expect("fixture has a cache-write price")
            .as_micro_usd();
        (0..=10)
            .find(|reads| write + reads * read <= (reads + 1) * uncached)
            .expect("documented fixture prices break even within ten reads")
    }

    let five_minute = fixture(300, 3_750_000);
    let one_hour = fixture(3_600, 6_000_000);

    for (model, ttl_seconds, write_price, break_even_reads) in [
        (&five_minute, 300, 3_750_000, 1),
        (&one_hour, 3_600, 6_000_000, 2),
    ] {
        let policy = model.cache_policy.expect("fixture publishes cache policy");
        assert_eq!(
            policy.kind,
            tau_proto::ProviderCacheKind::ExplicitBreakpoint
        );
        assert_eq!(policy.renewal, tau_proto::ProviderCacheRenewal::Read);
        assert_eq!(
            policy.output_floor,
            tau_proto::ProviderCacheOutputFloor::Zero
        );
        assert!(matches!(
            policy.ttl,
            tau_proto::ProviderCacheTtl::SlidingKnown { seconds }
                if seconds.get() == ttl_seconds
        ));
        assert_eq!(
            (
                model
                    .est_uncached_input_cost_1m_usd
                    .expect("fixture has an uncached price")
                    .as_micro_usd(),
                model
                    .est_cached_input_cost_1m_usd
                    .expect("fixture has a cache-read price")
                    .as_micro_usd(),
                model
                    .est_cache_write_input_cost_1m_usd
                    .expect("fixture has a cache-write price")
                    .as_micro_usd(),
            ),
            (3_000_000, 300_000, write_price)
        );
        assert_eq!(first_break_even_read(model), break_even_reads);
        assert!(
            model_snapshot(FakeModelCapabilities::default())
                .models
                .iter()
                .all(|published| published.id != model.id),
            "Anthropic policy fixtures must remain absent from dispatchable models"
        );
    }
}

/// Ensures standalone-compaction capability stays disabled for ordinary
/// scenarios and is published only by each dedicated closed action type.
#[test]
fn standalone_compaction_capability_requires_a_dedicated_action() {
    let v1 = ScenarioConfig::V1(ScenarioV1::text_v1("ordinary", "ordinary"));
    let ordinary_v2 = ScenarioConfig::V2(ScenarioV2::new(
        "ordinary-v2",
        vec![ScenarioLaneV2 {
            ctx_id: "ordinary".to_owned(),
            actions: vec![ScenarioActionV2::Text {
                user_text: "ordinary".to_owned(),
                response: "ordinary".to_owned(),
            }],
        }],
    ));
    for scenario in [&v1, &ordinary_v2] {
        assert!(!scenario.enables_standalone_compaction());
        assert!(
            !model_snapshot(FakeModelCapabilities {
                standalone_compaction: scenario.enables_standalone_compaction(),
                ..FakeModelCapabilities::default()
            })
            .models[0]
                .supports_standalone_compaction
        );
    }

    for action in [
        ScenarioActionV2::StandaloneCompaction {
            narrative: "Goal:\nsummary".to_owned(),
        },
        ScenarioActionV2::StandaloneOpaqueCompaction,
        ScenarioActionV2::StandaloneCompactionError {
            failure_kind: ProviderFailureKind::RequestRejected,
            error: "rejected".to_owned(),
        },
        ScenarioActionV2::StandaloneCompactionHold { timeout_ms: 100 },
    ] {
        let scenario = ScenarioConfig::V2(ScenarioV2::new(
            "standalone-enabled",
            vec![ScenarioLaneV2 {
                ctx_id: "standalone".to_owned(),
                actions: vec![action],
            }],
        ));
        assert!(scenario.enables_standalone_compaction());
        assert!(
            model_snapshot(FakeModelCapabilities {
                standalone_compaction: scenario.enables_standalone_compaction(),
                ..FakeModelCapabilities::default()
            })
            .models[0]
                .supports_standalone_compaction
        );
    }
}

/// Ensures the closed sibling-shell scenario can select either capability mode
/// while ordinary scenarios retain the default one-call capability.
#[test]
fn parallel_tool_capability_requires_a_dedicated_action() {
    let ordinary = ScenarioConfig::V2(ScenarioV2::new(
        "ordinary",
        vec![ScenarioLaneV2 {
            ctx_id: "ordinary".to_owned(),
            actions: vec![ScenarioActionV2::Text {
                user_text: "ordinary".to_owned(),
                response: "done".to_owned(),
            }],
        }],
    ));
    assert!(!ordinary.supports_parallel_tool_calls());

    let parallel = ScenarioConfig::V2(parallel_shell_scenario(
        PathBuf::from("/fixture/tau-e2e-shell-probe"),
        true,
    ));
    assert!(parallel.supports_parallel_tool_calls());
    let violating = ScenarioConfig::V2(parallel_shell_scenario(
        PathBuf::from("/fixture/tau-e2e-shell-probe"),
        false,
    ));
    assert!(!violating.supports_parallel_tool_calls());
}

/// Ensures the sibling-shell grammar rejects a relative probe_executable and
/// duplicate call identities before the fake provider publishes a model.
#[test]
fn parallel_shell_scenario_rejects_malformed_authority() {
    let valid = parallel_shell_scenario(PathBuf::from("/fixture/tau-e2e-shell-probe"), true);
    validation::validate_v2(&valid).expect("closed parallel shell scenario is valid");

    let relative = parallel_shell_scenario(PathBuf::from("tau-e2e-shell-probe"), true);
    assert!(
        validation::validate_v2(&relative)
            .expect_err("relative probe_executable")
            .to_string()
            .contains("absolute probe_executable")
    );

    let mut duplicate = valid.clone();
    let ScenarioActionV2::CoreShellParallelCalls { call_ids, .. } =
        &mut duplicate.lanes[0].actions[0]
    else {
        panic!("parallel call action");
    };
    call_ids[1] = call_ids[0].clone();
    for action in &mut duplicate.lanes[0].actions[1..] {
        match action {
            ScenarioActionV2::CoreShellParallelWaits { call_ids, .. }
            | ScenarioActionV2::CoreShellParallelResult { call_ids, .. } => {
                call_ids[1] = call_ids[0].clone();
            }
            _ => {}
        }
    }
    assert!(
        validation::validate_v2(&duplicate)
            .expect_err("duplicate shell id")
            .to_string()
            .contains("unique ids")
    );

    let mut extra = valid.clone();
    extra.lanes[0].actions.push(ScenarioActionV2::Text {
        user_text: "extra".to_owned(),
        response: "extra".to_owned(),
    });
    assert!(
        validation::validate_v2(&extra)
            .expect_err("extra action")
            .to_string()
            .contains("calls, waits, and result only")
    );

    let mut wait_mismatch = valid.clone();
    let ScenarioActionV2::CoreShellParallelWaits { call_ids, .. } =
        &mut wait_mismatch.lanes[0].actions[1]
    else {
        panic!("parallel waits");
    };
    call_ids[0] = "wrong-shell".into();
    assert!(
        validation::validate_v2(&wait_mismatch)
            .expect_err("wait correlation")
            .to_string()
            .contains("action correlation mismatch")
    );

    let mut wait_duplicate = valid.clone();
    let ScenarioActionV2::CoreShellParallelWaits { wait_call_id, .. } =
        &mut wait_duplicate.lanes[0].actions[1]
    else {
        panic!("parallel waits");
    };
    *wait_call_id = "parallel-shell-1".into();
    let duplicated_wait_id = wait_call_id.clone();
    let ScenarioActionV2::CoreShellParallelResult { wait_call_id, .. } =
        &mut wait_duplicate.lanes[0].actions[2]
    else {
        panic!("parallel result");
    };
    *wait_call_id = duplicated_wait_id;
    assert!(
        validation::validate_v2(&wait_duplicate)
            .expect_err("duplicate wait id")
            .to_string()
            .contains("unique id")
    );

    let mut result_mismatch = valid;
    let ScenarioActionV2::CoreShellParallelResult { wait_call_id, .. } =
        &mut result_mismatch.lanes[0].actions[2]
    else {
        panic!("parallel result");
    };
    *wait_call_id = "wrong-wait".into();
    assert!(
        validation::validate_v2(&result_mismatch)
            .expect_err("result correlation")
            .to_string()
            .contains("action correlation mismatch")
    );
}

fn parallel_shell_scenario(probe_executable: PathBuf, advertise_parallel: bool) -> ScenarioV2 {
    let call_ids = std::array::from_fn(|index| format!("parallel-shell-{}", index + 1).into());
    let wait_call_id = ToolCallId::from("parallel-wait-all");
    ScenarioV2::new(
        "parallel-shell",
        vec![ScenarioLaneV2 {
            ctx_id: "parallel-shell".to_owned(),
            actions: vec![
                ScenarioActionV2::CoreShellParallelCalls {
                    user_text: "parallel".to_owned(),
                    advertise_parallel,
                    call_ids: call_ids.clone(),
                    probe_executable,
                },
                ScenarioActionV2::CoreShellParallelWaits {
                    user_text: "parallel".to_owned(),
                    advertise_parallel,
                    call_ids: call_ids.clone(),
                    wait_call_id: wait_call_id.clone(),
                },
                ScenarioActionV2::CoreShellParallelResult {
                    user_text: "parallel".to_owned(),
                    advertise_parallel,
                    call_ids,
                    wait_call_id,
                    response: "done".to_owned(),
                },
            ],
        }],
    )
}

fn mixed_wait_all_scenario(probe_executable: PathBuf) -> ScenarioV2 {
    let wait_call_id = ToolCallId::from("mixed-wait-all");
    let success_call_id = ToolCallId::from("mixed-shell-success");
    let error_call_id = ToolCallId::from("mixed-workdir-error");
    ScenarioV2::new(
        "mixed-wait-all",
        vec![ScenarioLaneV2 {
            ctx_id: "mixed-wait-all".to_owned(),
            actions: vec![
                ScenarioActionV2::WaitAllMixedCalls {
                    user_text: "mixed".to_owned(),
                    wait_call_id: wait_call_id.clone(),
                    success_call_id: success_call_id.clone(),
                    error_call_id: error_call_id.clone(),
                    probe_executable,
                },
                ScenarioActionV2::WaitAllMixedResult {
                    user_text: "mixed".to_owned(),
                    wait_call_id,
                    success_call_id,
                    error_call_id,
                    response: "done".to_owned(),
                },
            ],
        }],
    )
}

/// Keeps the mixed plural-wait grammar closed around one correlated pair,
/// absolute helper authority, and three distinct bounded provider call IDs.
#[test]
fn mixed_wait_all_scenario_requires_one_correlated_closed_pair() {
    let valid = mixed_wait_all_scenario(PathBuf::from("/fixture/tau-e2e-shell-probe"));
    validation::validate_v2(&valid).expect("closed mixed wait-all scenario is valid");

    let mut mismatched = valid.clone();
    let ScenarioActionV2::WaitAllMixedResult {
        success_call_id, ..
    } = &mut mismatched.lanes[0].actions[1]
    else {
        panic!("mixed result action");
    };
    *success_call_id = "wrong-success".into();
    assert!(validation::validate_v2(&mismatched).is_err());

    let mut duplicate = valid.clone();
    let ScenarioActionV2::WaitAllMixedCalls {
        error_call_id,
        success_call_id,
        ..
    } = &mut duplicate.lanes[0].actions[0]
    else {
        panic!("mixed calls action");
    };
    *error_call_id = success_call_id.clone();
    let duplicated_error_id = error_call_id.clone();
    let ScenarioActionV2::WaitAllMixedResult { error_call_id, .. } =
        &mut duplicate.lanes[0].actions[1]
    else {
        panic!("mixed result action");
    };
    *error_call_id = duplicated_error_id;
    assert!(validation::validate_v2(&duplicate).is_err());

    let relative = mixed_wait_all_scenario(PathBuf::from("tau-e2e-shell-probe"));
    assert!(validation::validate_v2(&relative).is_err());

    let mut extra = valid;
    extra.lanes[0].actions.push(ScenarioActionV2::Text {
        user_text: "extra".to_owned(),
        response: "extra".to_owned(),
    });
    assert!(validation::validate_v2(&extra).is_err());
}

/// Rejects a typed-image sequence whose call and result identities differ so
/// the image fixture cannot become a general-purpose action grammar.
#[test]
fn typed_image_scenario_requires_one_correlated_closed_lane() {
    let scenario = ScenarioV2::new(
        "invalid-typed-image",
        vec![ScenarioLaneV2 {
            ctx_id: "typed-image".to_owned(),
            actions: vec![
                ScenarioActionV2::TypedImageToolCall {
                    user_text: "inspect".to_owned(),
                    call_id: "call-a".into(),
                },
                ScenarioActionV2::TypedImageToolResult {
                    call_id: "call-b".into(),
                    response: "live".to_owned(),
                },
                ScenarioActionV2::TypedImageReplay {
                    user_text: "continue".to_owned(),
                    call_id: "call-a".into(),
                    response: "replayed".to_owned(),
                },
            ],
        }],
    );
    assert!(
        validation::validate_v2(&scenario).is_err(),
        "typed-image action identities must remain correlated"
    );
}

fn output_length_scenario(user_text: &str, reasoning: &str, response: &str) -> ScenarioV2 {
    ScenarioV2::new(
        "output-length",
        vec![ScenarioLaneV2 {
            ctx_id: "output-length".to_owned(),
            actions: vec![
                ScenarioActionV2::OutputLengthReasoning {
                    user_text: user_text.to_owned(),
                    reasoning: reasoning.to_owned(),
                    report_usage: false,
                },
                ScenarioActionV2::OutputLengthContinuation {
                    user_text: user_text.to_owned(),
                    reasoning: reasoning.to_owned(),
                    response: response.to_owned(),
                    report_usage: false,
                },
            ],
        }],
    )
}

/// The output-limit fixture admits only one bounded correlated source/successor
/// pair, so it cannot become a general continuation grammar.
#[test]
fn output_length_scenario_requires_one_correlated_closed_pair() {
    let valid = output_length_scenario("user", "reasoning", "answer");
    assert!(validation::validate_v2(&valid).is_ok());

    let mut mismatch = valid.clone();
    let ScenarioActionV2::OutputLengthContinuation { reasoning, .. } =
        &mut mismatch.lanes[0].actions[1]
    else {
        unreachable!("constructed output-length continuation");
    };
    *reasoning = "other".to_owned();
    assert!(validation::validate_v2(&mismatch).is_err());

    let mut extra = valid;
    extra.lanes[0].actions.push(ScenarioActionV2::Text {
        user_text: "extra".to_owned(),
        response: "extra".to_owned(),
    });
    assert!(validation::validate_v2(&extra).is_err());
}

/// HumanUi fixture matching projects expected typed text and never invents a
/// decoded semantic value from the intentionally non-injective provider form.
#[test]
fn human_ui_fixture_projection_preserves_bytes_and_exposes_close_collision() {
    assert_eq!(
        project_fixture_human_ui_user_prompt(" \t<x> &amp; \"q\" 'a'\n雪\u{202e}  "),
        "<user> \t<x> &amp; \"q\" 'a'\n雪\u{202e}  </user>"
    );
    assert_eq!(
        project_fixture_human_ui_user_prompt("</user>"),
        project_fixture_human_ui_user_prompt("&lt;/user&gt;"),
        "exact-close framing is one-way and the fixture must not decode it"
    );
    assert!(fixture_user_text_matches(
        "<user>&lt;/user&gt;</user>",
        "</user>"
    ));
    assert!(fixture_user_text_matches("internal raw", "internal raw"));
}

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

/// Ensures the fake provider admits two dummy pairs only for the one closed
/// disconnect-repair-replacement lifecycle and rejects reordered, repeated,
/// mismatched, or extra pairs.
#[test]
fn v2_exit_once_dummy_lifecycle_grammar_is_closed() {
    let scenario = ScenarioV2::new(
        "exit-once",
        vec![ScenarioLaneV2 {
            ctx_id: "exit-once-lane".to_owned(),
            actions: vec![
                ScenarioActionV2::DummyToolCall {
                    user_text: "disconnect".to_owned(),
                    call_id: "disconnect-call".into(),
                },
                ScenarioActionV2::DummyToolRepair {
                    user_text: "disconnect".to_owned(),
                    call_id: "disconnect-call".into(),
                    diagnostic: "disconnect diagnostic".to_owned(),
                    response: "disconnect observed".to_owned(),
                },
                ScenarioActionV2::DummyToolCall {
                    user_text: "replacement".to_owned(),
                    call_id: "replacement-call".into(),
                },
                ScenarioActionV2::DummyToolResult {
                    user_text: "replacement".to_owned(),
                    call_id: "replacement-call".into(),
                    response: "replacement succeeded".to_owned(),
                },
            ],
        }],
    );
    assert!(
        FakeConfig {
            scenario: ScenarioConfig::V2(scenario.clone())
        }
        .validate()
        .is_ok()
    );

    let mut reordered = scenario.clone();
    reordered.lanes[0].actions.swap(1, 2);
    let mut repeated = scenario.clone();
    let ScenarioActionV2::DummyToolCall { call_id, .. } = &mut repeated.lanes[0].actions[2] else {
        unreachable!()
    };
    *call_id = "disconnect-call".into();
    let mut mismatched = scenario.clone();
    let ScenarioActionV2::DummyToolResult { call_id, .. } = &mut mismatched.lanes[0].actions[3]
    else {
        unreachable!()
    };
    *call_id = "wrong-call".into();
    let mut extra = scenario.clone();
    extra.lanes[0]
        .actions
        .push(ScenarioActionV2::DummyToolCall {
            user_text: "extra".to_owned(),
            call_id: "extra-call".into(),
        });
    extra.lanes[0]
        .actions
        .push(ScenarioActionV2::DummyToolResult {
            user_text: "extra".to_owned(),
            call_id: "extra-call".into(),
            response: "extra".to_owned(),
        });
    let mut extra_lane = scenario.clone();
    extra_lane.lanes.push(ScenarioLaneV2 {
        ctx_id: "unrelated".to_owned(),
        actions: vec![ScenarioActionV2::Text {
            user_text: "unrelated".to_owned(),
            response: "unrelated".to_owned(),
        }],
    });
    for invalid in [reordered, repeated, mismatched, extra, extra_lane] {
        assert!(
            FakeConfig {
                scenario: ScenarioConfig::V2(invalid)
            }
            .validate()
            .is_err()
        );
    }
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
            .record_dummy_repair_event(&tool_error("call", "repair"))
            .is_err()
    );
    assert!(state.repair_progress.is_none());
    assert!(
        state
            .record_dummy_repair_event(&provider_tool_error("wrong", "repair"))
            .is_err()
    );
    assert!(state.repair_progress.is_none());
    state
        .record_dummy_repair_event(&provider_tool_error("call", "repair"))
        .expect("exact provider error");
    assert!(
        state
            .record_dummy_repair_event(&provider_tool_error("call", "repair"))
            .is_err()
    );
    state
        .record_dummy_repair_event(&tool_error("call", "repair"))
        .expect("exact tool error");
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

/// Ensures the sole exit-once replacement continuation retains exactly the
/// repaired first error and the second normal result, rejecting omissions,
/// mutations, and extra terminals.
#[test]
fn v2_exit_once_replacement_continuation_requires_exact_two_terminals() {
    let scenario = exit_once_scenario();
    let action = scenario.lanes[0].actions[3].clone();
    let agent = tau_proto::AgentId::parse("agent").expect("agent id");
    let mut state = FakeState::default();
    state.scenario = Some(ScenarioConfig::V2(scenario));
    state.lane_cursors = vec![3];
    state.agent_lanes = HashMap::from([(agent.clone(), 0)]);
    let mut prompt = prompt_for(&agent, "replacement", None);
    prompt
        .context
        .blocks
        .push(tau_proto::ContextBlock::ToolResults(
            tau_proto::ToolResultsBlock {
                items: vec![
                    error_tool_result("disconnect-call", "disconnect diagnostic"),
                    tool_result("replacement-call", "restart succeeded"),
                ],
            },
        ));
    let exact = prompt.clone();

    latest_tool_results_mut(&mut prompt).items.remove(0);
    assert!(state.validate_v2_action(3, &prompt, &action).is_err());
    prompt = exact.clone();
    latest_tool_results_mut(&mut prompt).items[0] =
        error_tool_result("disconnect-call", "mutated diagnostic");
    assert!(state.validate_v2_action(3, &prompt, &action).is_err());
    prompt = exact.clone();
    latest_tool_results_mut(&mut prompt).items[1] = tool_result("replacement-call", "wrong");
    assert!(state.validate_v2_action(3, &prompt, &action).is_err());
    prompt = exact.clone();
    latest_tool_results_mut(&mut prompt)
        .items
        .push(tool_result("extra-call", "restart succeeded"));
    assert!(state.validate_v2_action(3, &prompt, &action).is_err());
    state
        .validate_and_commit_v2_action(0, 3, &exact, &action)
        .expect("exact repaired error plus replacement success commits");
    assert_eq!(state.lane_cursors, [4]);
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
            scenario: ScenarioConfig::V2(v2_action(action(vec![WatchNotificationV2::Response {
                content: "done".to_owned(),
            }])))
        }
        .validate()
        .is_ok()
    );
    for notifications in [
        Vec::new(),
        vec![
            WatchNotificationV2::Response {
                content: "done".to_owned(),
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

/// Accepts the prompt-before-response chain and rejects response-first or
/// duplicate-prompt admission.
#[test]
fn v2_watch_notification_chains_enforce_prompt_before_response() {
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

    let mut ordered = state();
    for message in [
        watch_prompt(&parent, &child, "work"),
        watch_response(&parent, &child, "done"),
    ] {
        ordered
            .record_watch_notification(&message)
            .expect("valid content-chain order");
    }
    assert_eq!(ordered.watch_notifications[&parent].len(), 2);

    let mut response_first = state();
    assert!(
        response_first
            .record_watch_notification(&watch_response(&parent, &child, "done"))
            .is_err()
    );
    assert!(response_first.watch_notifications.is_empty());

    let mut duplicate_prompt = state();
    duplicate_prompt
        .record_watch_notification(&watch_prompt(&parent, &child, "work"))
        .expect("first prompt");
    assert!(
        duplicate_prompt
            .record_watch_notification(&watch_prompt(&parent, &child, "work"))
            .is_err()
    );
    assert_eq!(duplicate_prompt.watch_notifications[&parent].len(), 1);
}

/// Rejects unrelated, malformed, and excess live watch records
/// without advancing the lane cursor or admitting the bad record.
#[test]
fn v2_watch_runtime_mismatches_leave_the_action_unconsumed() {
    let notifications = vec![WatchNotificationV2::Response {
        content: "done".to_owned(),
    }];
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

    let mut wrong_sender = watch_response(
        &parent,
        &tau_proto::AgentId::parse("other").expect("other id"),
        "done",
    );
    assert!(state.record_watch_notification(&wrong_sender).is_err());
    assert!(state.watch_notifications.is_empty());

    wrong_sender.sender_id = child.clone();
    state
        .record_watch_notification(&wrong_sender)
        .expect("exact response notification");
    assert_eq!(state.watch_notifications[&parent].len(), 1);

    let wrong_content = watch_response(&parent, &child, "other");
    assert!(state.record_watch_notification(&wrong_content).is_err());
    assert_eq!(state.watch_notifications[&parent].len(), 1);
    assert!(state.record_watch_notification(&wrong_sender).is_err());
    assert_eq!(state.watch_notifications[&parent].len(), 1);
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

/// Ensures only an explicitly allowlisted one-lane public-PTY scenario can bind
/// a harness-minted interactive prompt correlation.
#[test]
fn v2_dynamic_public_pty_lane_binding_is_explicitly_allowlisted() {
    let prompt = prompt_for(
        &tau_proto::AgentId::parse("main").expect("agent id"),
        "<user>turn</user>",
        Some("ui-prompt-generated"),
    );
    for name in PUBLIC_PTY_DYNAMIC_LANE_SCENARIOS {
        let mut state = FakeState::default();
        state.scenario = Some(ScenarioConfig::V2(ScenarioV2::new(
            *name,
            vec![ScenarioLaneV2 {
                ctx_id: "configured-placeholder".to_owned(),
                actions: vec![ScenarioActionV2::Text {
                    user_text: "<user>turn</user>".to_owned(),
                    response: "done".to_owned(),
                }],
            }],
        )));
        state.lane_cursors = vec![0];
        assert_eq!(
            state
                .select_v2_lane(&prompt)
                .expect("allowlisted public PTY binds"),
            0
        );
    }

    let mut rejected = FakeState::default();
    rejected.scenario = Some(ScenarioConfig::V2(ScenarioV2::new(
        "diagnostic-name-only",
        vec![ScenarioLaneV2 {
            ctx_id: "configured-placeholder".to_owned(),
            actions: vec![ScenarioActionV2::Text {
                user_text: "<user>turn</user>".to_owned(),
                response: "done".to_owned(),
            }],
        }],
    )));
    rejected.lane_cursors = vec![0];
    assert!(rejected.select_v2_lane(&prompt).is_err());
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
                        presentation: Default::default(),
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
        agent_prompt_id: "ap-test-0"
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
        agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
        session_id: "session"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
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
        hosted_tools: Vec::new(),
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
                    role: "worker".to_owned(),
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
        message_id: tau_proto::AgentMessageId::parse("watch-response")
            .expect("test identifier must satisfy its grammar"),
        sender_id: child.clone(),
        sender_session_id: None,
        recipient_id: parent.clone(),
        kind: tau_proto::AgentMessageKind::WatchResponse,
        watch_provider_status: None,
        watch_work_status: None,
        watch_long_wait: None,
        watch_lifecycle: None,
        message: content.to_owned(),
    }
}

fn watch_prompt(
    parent: &tau_proto::AgentId,
    child: &tau_proto::AgentId,
    content: &str,
) -> AgentMessageReceived {
    AgentMessageReceived {
        message_id: tau_proto::AgentMessageId::parse("watch-prompt")
            .expect("test identifier must satisfy its grammar"),
        sender_id: child.clone(),
        sender_session_id: None,
        recipient_id: parent.clone(),
        kind: tau_proto::AgentMessageKind::WatchPrompt,
        watch_provider_status: None,
        watch_work_status: None,
        watch_long_wait: None,
        watch_lifecycle: None,
        message: content.to_owned(),
    }
}

fn prompt_for(
    agent_id: &tau_proto::AgentId,
    user_text: &str,
    ctx_id: Option<&str>,
) -> tau_proto::AgentPromptCreated {
    tau_proto::AgentPromptCreated {
        agent_prompt_id: "ap-test"
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
        agent_id: agent_id.clone(),
        session_id: "session"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
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
        hosted_tools: Vec::new(),
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

/// The optional release accepts exactly the late and coalesced scheduler
/// shapes, while duplicate, reordered, and unrelated inputs fail closed.
#[test]
fn optional_barrier_classification_is_exact() {
    fn append_user(prompt: &mut tau_proto::AgentPromptCreated, text: &str) {
        prompt
            .context
            .blocks
            .push(tau_proto::ContextBlock::UserInput(
                tau_proto::UserInputBlock {
                    items: vec![ContextItem::Message(MessageItem {
                        role: ContextRole::User,
                        content: vec![ContentPart::Text {
                            text: text.to_owned(),
                        }],
                        phase: None,
                        responses_raw_json: None,
                    })],
                },
            ));
    }

    let agent_id = tau_proto::AgentId::parse("main").expect("test agent id");
    let late = prompt_for(&agent_id, "raw call", Some("sender"));
    assert_eq!(
        optional_barrier_is_coalesced(&late, "raw call", "release"),
        Ok(false)
    );

    let mut coalesced = late.clone();
    append_user(&mut coalesced, "release");
    assert_eq!(
        optional_barrier_is_coalesced(&coalesced, "raw call", "release"),
        Ok(true)
    );

    let mut duplicate = coalesced.clone();
    append_user(&mut duplicate, "release");
    assert!(optional_barrier_is_coalesced(&duplicate, "raw call", "release").is_err());

    let mut reordered = coalesced;
    append_user(&mut reordered, "unexpected");
    assert!(optional_barrier_is_coalesced(&reordered, "raw call", "release").is_err());

    let unexpected = prompt_for(&agent_id, "unexpected", Some("sender"));
    assert!(optional_barrier_is_coalesced(&unexpected, "raw call", "release").is_err());

    let release_without_prior = prompt_for(&agent_id, "release", Some("sender"));
    assert!(optional_barrier_is_coalesced(&release_without_prior, "raw call", "release").is_err());

    let mut interposed = late;
    append_user(&mut interposed, "unexpected");
    append_user(&mut interposed, "release");
    assert!(optional_barrier_is_coalesced(&interposed, "raw call", "release").is_err());
}

/// Both scheduler branches consume and checkpoint the exact action span, while
/// the coalesced branch releases the already-waiting barrier outputs in order.
#[test]
fn optional_barrier_consumes_exact_late_and_coalesced_spans() {
    fn scenario() -> ScenarioV2 {
        ScenarioV2::new(
            "optional-barrier-unit",
            vec![ScenarioLaneV2 {
                ctx_id: "sender".to_owned(),
                actions: vec![
                    ScenarioActionV2::ProviderContextRawMessageResultOrBarrier {
                        call_id: "raw-id".into(),
                        raw_text: "raw body".to_owned(),
                        prior_user_text: "raw call".to_owned(),
                        response: "raw complete".to_owned(),
                        release_user_text: "release".to_owned(),
                        barrier: "release-barrier".to_owned(),
                        participants: 2,
                        barrier_response: "sender released".to_owned(),
                    },
                    ScenarioActionV2::BarrierText {
                        user_text: "release".to_owned(),
                        barrier: "release-barrier".to_owned(),
                        participants: 2,
                        response: "sender released".to_owned(),
                    },
                ],
            }],
        )
    }

    fn prompt(include_release: bool) -> tau_proto::AgentPromptCreated {
        let agent_id = tau_proto::AgentId::parse("main").expect("test agent id");
        let mut prompt = prompt_for(&agent_id, "raw call", Some("sender"));
        prompt
            .context
            .blocks
            .push(tau_proto::ContextBlock::ToolResults(
                tau_proto::ToolResultsBlock {
                    items: vec![tool_result("raw-id", "raw message emitted")],
                },
            ));
        if include_release {
            prompt
                .context
                .blocks
                .push(tau_proto::ContextBlock::UserInput(
                    tau_proto::UserInputBlock {
                        items: vec![ContextItem::Message(MessageItem {
                            role: ContextRole::User,
                            content: vec![ContentPart::Text {
                                text: "release".to_owned(),
                            }],
                            phase: None,
                            responses_raw_json: None,
                        })],
                    },
                ));
        }
        prompt
    }

    fn state(
        tempdir: &tempfile::TempDir,
        suffix: &str,
    ) -> (FakeState, std::path::PathBuf, std::path::PathBuf) {
        let checkpoint = tempdir.path().join(format!("{suffix}-cursor.json"));
        let trace_path = tempdir.path().join(format!("{suffix}-trace"));
        let mut state = FakeState::default();
        state.scenario = Some(ScenarioConfig::V2(scenario()));
        state.lane_cursors = vec![0];
        state.checkpoint = Some(checkpoint.clone());
        state.trace = Some(Arc::new(Mutex::new(
            File::create(&trace_path).expect("create trace"),
        )));
        (state, checkpoint, trace_path)
    }

    fn checkpoint_cursor(path: &std::path::Path) -> usize {
        let checkpoint: CursorCheckpoint =
            serde_json::from_slice(&std::fs::read(path).expect("read checkpoint"))
                .expect("decode checkpoint");
        checkpoint.cursors[0]
    }

    fn trace(state: &FakeState, path: &std::path::Path) -> String {
        state
            .trace
            .as_ref()
            .expect("trace")
            .lock()
            .expect("trace lock")
            .flush()
            .expect("flush trace");
        std::fs::read_to_string(path).expect("read trace")
    }

    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let action = scenario().lanes[0].actions[0].clone();
    let late_prompt = prompt(false);
    let (mut late, late_checkpoint, late_trace) = state(&tempdir, "late");
    let late_emission = late
        .consume_optional_raw_result_barrier(0, 0, 2, "sender", &late_prompt, &action)
        .expect("late raw result");
    assert!(
        matches!(late_emission, OptionalBarrierEmission::RawResult(response) if response == "raw complete")
    );
    assert_eq!(late.lane_cursors, [1]);
    assert_eq!(checkpoint_cursor(&late_checkpoint), 1);
    assert_eq!(trace(&late, &late_trace).matches(" matched ").count(), 1);
    assert!(late.barriers.is_empty());

    let coalesced_prompt = prompt(true);
    let (mut not_ready, not_ready_checkpoint, not_ready_trace) = state(&tempdir, "not-ready");
    assert!(
        not_ready
            .consume_optional_raw_result_barrier(0, 0, 2, "sender", &coalesced_prompt, &action)
            .is_err()
    );
    assert_eq!(not_ready.lane_cursors, [0]);
    assert!(not_ready.agent_lanes.is_empty());
    assert!(!not_ready_checkpoint.exists());
    let not_ready_trace = trace(&not_ready, &not_ready_trace);
    assert!(not_ready_trace.contains("every other barrier participant"));
    assert_eq!(not_ready_trace.matches(" matched ").count(), 0);
    assert!(not_ready.barriers.is_empty());

    let (mut coalesced, coalesced_checkpoint, coalesced_trace) = state(&tempdir, "coalesced");
    let target = prompt_for(
        &tau_proto::AgentId::parse("target").expect("target id"),
        "held",
        Some("target"),
    );
    assert!(matches!(
        coalesced
            .join_barrier(
                &target,
                "release-barrier".to_owned(),
                2,
                BarrierOutput::ParallelDummyTools(vec!["tool-a".into(), "tool-b".into()]),
            )
            .expect("stage target"),
        BarrierJoin::Pending
    ));
    let mut coalesced_prompt_with_history = coalesced_prompt.clone();
    coalesced_prompt_with_history.context.blocks.insert(
        1,
        tau_proto::ContextBlock::ToolResults(tau_proto::ToolResultsBlock {
            items: vec![tool_result("older-id", "older result")],
        }),
    );
    let coalesced_emission = coalesced
        .consume_optional_raw_result_barrier(
            0,
            0,
            2,
            "sender",
            &coalesced_prompt_with_history,
            &action,
        )
        .expect("coalesced raw result and barrier");
    let OptionalBarrierEmission::Barrier(completed) = coalesced_emission else {
        panic!("coalesced release must emit the completed barrier");
    };
    assert_eq!(completed.len(), 2);
    assert!(matches!(
        &completed[0].output,
        BarrierOutput::ParallelDummyTools(ids)
            if ids == &vec![
                tau_proto::ToolCallId::new("tool-a"),
                tau_proto::ToolCallId::new("tool-b")
            ]
    ));
    assert!(
        matches!(&completed[1].output, BarrierOutput::Text(response) if response == "sender released")
    );
    assert_eq!(coalesced.lane_cursors, [2]);
    assert_eq!(checkpoint_cursor(&coalesced_checkpoint), 2);
    let coalesced_trace = trace(&coalesced, &coalesced_trace);
    assert!(coalesced_trace.contains("actions=0,1"));
    assert_eq!(coalesced_trace.matches(" matched ").count(), 2);
    assert!(coalesced.barriers.is_empty());

    let (mut wrong_operation, checkpoint, trace_path) = state(&tempdir, "operation");
    let mut wrong_prompt = coalesced_prompt.clone();
    wrong_prompt.operation = tau_proto::PromptOperation::StandaloneCompaction;
    assert!(
        wrong_operation
            .consume_optional_raw_result_barrier(0, 0, 2, "sender", &wrong_prompt, &action,)
            .is_err()
    );
    assert_eq!(wrong_operation.lane_cursors, [0]);
    assert!(wrong_operation.agent_lanes.is_empty());
    assert!(!checkpoint.exists());
    assert!(trace(&wrong_operation, &trace_path).contains("prompt operation mismatch"));
    assert!(wrong_operation.barriers.is_empty());

    for (suffix, extra) in [
        ("same-call-duplicate", tool_result("raw-id", "inconsistent")),
        ("unrelated-result", tool_result("other-id", "other result")),
    ] {
        let (mut invalid, checkpoint, trace_path) = state(&tempdir, suffix);
        let mut invalid_prompt = prompt(true);
        let Some(tau_proto::ContextBlock::ToolResults(results)) = invalid_prompt
            .context
            .blocks
            .iter_mut()
            .find(|block| matches!(block, tau_proto::ContextBlock::ToolResults(_)))
        else {
            unreachable!("prompt contains one tool-result block")
        };
        results.items.push(extra);
        assert!(
            invalid
                .consume_optional_raw_result_barrier(0, 0, 2, "sender", &invalid_prompt, &action,)
                .is_err()
        );
        assert_eq!(invalid.lane_cursors, [0]);
        assert!(invalid.agent_lanes.is_empty());
        assert!(!checkpoint.exists());
        assert!(trace(&invalid, &trace_path).contains("raw message tool result mismatch"));
        assert!(invalid.barriers.is_empty());
    }

    let (mut newest_conflict, checkpoint, trace_path) = state(&tempdir, "newest-block-conflict");
    let mut newest_conflict_prompt = coalesced_prompt;
    newest_conflict_prompt
        .context
        .blocks
        .push(tau_proto::ContextBlock::ToolResults(
            tau_proto::ToolResultsBlock {
                items: vec![tool_result("other-id", "newest conflicting result")],
            },
        ));
    assert!(
        newest_conflict
            .consume_optional_raw_result_barrier(
                0,
                0,
                2,
                "sender",
                &newest_conflict_prompt,
                &action,
            )
            .is_err()
    );
    assert_eq!(newest_conflict.lane_cursors, [0]);
    assert!(newest_conflict.agent_lanes.is_empty());
    assert!(!checkpoint.exists());
    assert!(trace(&newest_conflict, &trace_path).contains("raw message tool result mismatch"));
    assert!(newest_conflict.barriers.is_empty());
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
        presentation: Default::default(),
        call_id: call_id.into(),
        tool_type: ToolType::Function,
        status: tau_proto::ToolResultStatus::Success,
        output: tau_proto::ToolResponse::from_cbor(&raw),
        provider_content: Vec::new(),
    }
}

fn tool_result(call_id: &str, text: &str) -> tau_proto::ToolResultItem {
    tau_proto::ToolResultItem {
        presentation: Default::default(),
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
        presentation: Default::default(),
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

fn exit_once_scenario() -> ScenarioV2 {
    ScenarioV2::new(
        "exit-once",
        vec![ScenarioLaneV2 {
            ctx_id: "exit-once-lane".to_owned(),
            actions: vec![
                ScenarioActionV2::DummyToolCall {
                    user_text: "disconnect".to_owned(),
                    call_id: "disconnect-call".into(),
                },
                ScenarioActionV2::DummyToolRepair {
                    user_text: "disconnect".to_owned(),
                    call_id: "disconnect-call".into(),
                    diagnostic: "disconnect diagnostic".to_owned(),
                    response: "disconnect observed".to_owned(),
                },
                ScenarioActionV2::DummyToolCall {
                    user_text: "replacement".to_owned(),
                    call_id: "replacement-call".into(),
                },
                ScenarioActionV2::DummyToolResult {
                    user_text: "replacement".to_owned(),
                    call_id: "replacement-call".into(),
                    response: "replacement succeeded".to_owned(),
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

/// The placement grammar accepts only the two fully cross-bound non-tool and
/// parallel-tool shapes and rejects swapped or mismatched fields.
#[test]
fn v2_provider_context_placement_accepts_only_two_cross_bound_shapes() {
    let sender = crate::ScenarioLaneV2 {
        ctx_id: "sender".to_owned(),
        actions: vec![
            ScenarioActionV2::MessageCall {
                user_text: "typed call".to_owned(),
                call_id: "typed-id".into(),
                message: "typed body".to_owned(),
            },
            ScenarioActionV2::MessageSenderResult {
                call_id: "typed-id".into(),
                message: "typed body".to_owned(),
                response: "typed sent".to_owned(),
            },
            ScenarioActionV2::ProviderContextRawMessageCall {
                user_text: "raw call".to_owned(),
                call_id: "raw-id".into(),
                raw_text: "raw body".to_owned(),
            },
            ScenarioActionV2::ProviderContextRawMessageResultOrBarrier {
                call_id: "raw-id".into(),
                raw_text: "raw body".to_owned(),
                prior_user_text: "raw call".to_owned(),
                response: "raw sent".to_owned(),
                release_user_text: "release".to_owned(),
                barrier: "release".to_owned(),
                participants: 2,
                barrier_response: "released".to_owned(),
            },
            ScenarioActionV2::BarrierText {
                user_text: "release".to_owned(),
                barrier: "release".to_owned(),
                participants: 2,
                response: "released".to_owned(),
            },
        ],
    };
    let non_tool_target = crate::ScenarioLaneV2 {
        ctx_id: "target".to_owned(),
        actions: vec![
            ScenarioActionV2::BarrierText {
                user_text: "held".to_owned(),
                barrier: "release".to_owned(),
                participants: 2,
                response: "response".to_owned(),
            },
            ScenarioActionV2::MessageAndRawInboundAfterHeld {
                call_id: "typed-id".into(),
                message: "typed body".to_owned(),
                raw_text: "raw body".to_owned(),
                held_user_text: "held".to_owned(),
                response: "successor".to_owned(),
            },
        ],
    };
    let valid = ScenarioV2::new(
        "provider-context-shape",
        vec![sender.clone(), non_tool_target.clone()],
    );
    assert!(validate_v2(&valid).is_ok());

    let mut mismatched = valid.clone();
    let ScenarioActionV2::MessageAndRawInboundAfterHeld { raw_text, .. } =
        &mut mismatched.lanes[1].actions[1]
    else {
        unreachable!("known target action")
    };
    *raw_text = "other raw body".to_owned();
    assert!(validate_v2(&mismatched).is_err());

    let mut mismatched_release = valid.clone();
    let ScenarioActionV2::ProviderContextRawMessageResultOrBarrier {
        barrier_response, ..
    } = &mut mismatched_release.lanes[0].actions[3]
    else {
        unreachable!("known optional barrier action")
    };
    *barrier_response = "other release".to_owned();
    assert!(validate_v2(&mismatched_release).is_err());

    let mut swapped = valid.clone();
    swapped.lanes[1].actions[1] = ScenarioActionV2::MessageAndRawInboundAfterParallelTools {
        call_id: "typed-id".into(),
        message: "typed body".to_owned(),
        raw_text: "raw body".to_owned(),
        held_user_text: "held".to_owned(),
        tool_call_ids: vec!["tool-a".into(), "tool-b".into()],
        response: "successor".to_owned(),
    };
    assert!(validate_v2(&swapped).is_err());

    let parallel_target = crate::ScenarioLaneV2 {
        ctx_id: "target".to_owned(),
        actions: vec![
            ScenarioActionV2::BarrierParallelDummyTools {
                user_text: "held".to_owned(),
                barrier: "release".to_owned(),
                participants: 2,
                tool_call_ids: vec!["tool-a".into(), "tool-b".into()],
            },
            ScenarioActionV2::MessageAndRawInboundAfterParallelTools {
                call_id: "typed-id".into(),
                message: "typed body".to_owned(),
                raw_text: "raw body".to_owned(),
                held_user_text: "held".to_owned(),
                tool_call_ids: vec!["tool-a".into(), "tool-b".into()],
                response: "successor".to_owned(),
            },
        ],
    };
    let parallel = ScenarioV2::new("parallel-provider-context", vec![sender, parallel_target]);
    assert!(validate_v2(&parallel).is_ok());
    let mut duplicate = parallel;
    let ScenarioActionV2::BarrierParallelDummyTools { tool_call_ids, .. } =
        &mut duplicate.lanes[1].actions[0]
    else {
        unreachable!("known parallel barrier")
    };
    tool_call_ids[1] = tool_call_ids[0].clone();
    assert!(validate_v2(&duplicate).is_err());
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
