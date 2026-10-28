//! Fresh conversational main readiness must not depend on an unused worker
//! role.

use super::*;

/// A fresh UI can select a valid main despite an unavailable implicit worker,
/// while explicit-role, mainless, and headless startup retain strict
/// validation.
#[test]
fn fresh_main_ui_skips_only_unused_ordinary_role_skill_failure() {
    for (ui_demand, configured_main, explicit_role, succeeds) in [
        (true, true, false, true),
        (true, true, true, false),
        (true, false, false, false),
        (false, true, false, false),
    ] {
        let td = TempDir::new().expect("tempdir");
        let config_dir = td.path().join("config");
        let state_dir = td.path().join("state");
        std::fs::create_dir_all(&config_dir).expect("config directory");
        let priority = if configured_main { "100" } else { "null" };
        std::fs::write(
            config_dir.join("harness.yaml"),
            format!(
                r#"
agents:
  enable: false
  role_groups:
    engineer:
      roles:
        engineer-junior:
          enable: true
          order: 0
          required_skills: [missing-main-startup-regression-skill]
    coordination:
      roles:
        coordinator:
          enable: true
          order: 1
          main_priority: {priority}
"#
            ),
        )
        .expect("configuration");
        let captured_sink = Arc::new(Mutex::new(None));
        let hook_sink = captured_sink.clone();
        let result = echo_harness_with_dirs_start_reason_and_hook(
            "main-startup",
            &state_dir,
            tau_config::settings::TauDirs {
                config_dir: Some(config_dir),
                state_dir: Some(state_dir.clone()),
            },
            tau_proto::SessionStartReason::Initial,
            crate::HarnessStorageMode::Durable,
            Some(Box::new(move |h| {
                assert_eq!(h.config.selected_role, "engineer-junior");
                assert!(!h.session_initialized(&h.session_runtime.current_session_id));
                assert_eq!(
                    h.config.accepted_harness_settings.main_role(),
                    configured_main.then_some("coordinator")
                );
                if explicit_role {
                    h.config.accepted_harness_settings.startup_role =
                        Some("engineer-junior".to_owned());
                }
                if ui_demand {
                    let sink = connect_test_client(h, "startup-ui", tau_proto::ClientKind::Ui);
                    let client = crate::test_connection_id("startup-ui");
                    h.handle_client_message_disposition(
                        &client,
                        HarnessInputMessage::UiEnsureMainAgent,
                    )
                    .expect("main demand");
                    h.complete_subscription(
                        &client,
                        vec![EventSelector::Prefix(String::new())],
                        vec![EventSelector::Prefix(String::new())],
                    )
                    .expect("subscribe");
                    assert!(h.ui_runtime.main_demand_clients.contains(&client));
                    *hook_sink.lock().expect("sink slot") = Some(sink);
                }
            })),
        );
        if !succeeds {
            let error = match result {
                Ok(_) => panic!("used ordinary role must fail required-skill validation"),
                Err(error) => error,
            };
            assert!(
                error
                    .to_string()
                    .contains("selected/default role is unavailable"),
                "{error}"
            );
            continue;
        }
        let mut h = result.expect("unused ordinary role must not block main");
        assert_eq!(h.config.selected_role, "engineer-junior");
        assert!(!h.config.available_roles.contains_key("engineer-junior"));
        let main = h.main_agent_id().expect("canonical main").clone();
        let cid = &h.agent_runtime.agent_registry.agent_routes[&main];
        assert_eq!(
            h.agent_runtime.agent_registry.agents[cid]
                .identity
                .role
                .as_deref(),
            Some("coordinator")
        );
        assert_eq!(h.agent_runtime.agent_registry.agents.len(), 1);
        assert!(event_log_events(&h).iter().all(|event| !matches!(
            event,
            Event::AgentPromptCreated(_) | Event::AgentInferenceDispatchStarted(_)
        )));
        let sink = captured_sink.lock().expect("sink slot").take().expect("UI");
        let frames = sink.lock().expect("frames");
        let boundaries = frames
            .iter()
            .filter_map(|frame| match peel_inner_event(&frame.frame) {
                Some(Event::SessionReplayComplete(complete)) => Some(complete),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(boundaries.len(), 1);
        assert_eq!(boundaries[0].main_agent_id.as_ref(), Some(&main));
        drop(frames);
        h.shutdown().expect("shutdown");
    }
}
