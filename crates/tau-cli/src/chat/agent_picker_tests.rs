use super::{AgentPickerResolution, resolve_agent_picker};

fn auto_entry(runtime_state: tau_proto::AgentRuntimeState) -> tau_proto::SessionAgentListEntry {
    tau_proto::SessionAgentListEntry {
        agent_id: tau_proto::AgentId::parse("auto").expect("valid agent id"),
        lifecycle: tau_proto::SessionAgentLifecycle::Live {
            runtime_state,
            navigation_mode: tau_proto::AgentNavigationMode::ActiveAuto,
        },
        persistence: tau_proto::SessionAgentPersistence::Durable,
        facts: tau_proto::SessionAgentFacts::Available {
            started_at: Some(tau_proto::UnixMicros::new(1)),
            parent_agent: None,
            role: "engineer".to_owned(),
            display_name: None,
        },
    }
}

/// A running automatic agent that becomes idle between the displayed and fresh
/// snapshots must preserve selection/draft in the active picker, while the
/// all-agent picker must retain the initiating category and select it.
#[test]
fn picker_orchestration_revalidates_with_initiating_category() {
    let running = auto_entry(tau_proto::AgentRuntimeState::Running);
    let idle = auto_entry(tau_proto::AgentRuntimeState::Idle);
    let pick_auto = |rows: &str| {
        assert!(rows.contains("auto\tlive\trunning\tactive_auto\t"));
        assert!(rows.lines().all(|row| row.ends_with("\t$.00")));
        Ok(Some("auto\tlive\trunning\tactive_auto".to_owned()))
    };

    let active = resolve_agent_picker(
        vec![running.clone()],
        crate::list_agents::AgentPickerFilter::Active,
        |_| Some(tau_proto::EstimatedApiCost::default()),
        pick_auto,
        || Some(vec![idle.clone()]),
        || true,
        |agent_id| agent_id == "auto",
    );
    assert_eq!(
        active,
        AgentPickerResolution::Notice("selected agent is no longer available".to_owned())
    );

    let all = resolve_agent_picker(
        vec![running],
        crate::list_agents::AgentPickerFilter::All,
        |_| Some(tau_proto::EstimatedApiCost::default()),
        pick_auto,
        || Some(vec![idle]),
        || true,
        |agent_id| agent_id == "auto",
    );
    assert_eq!(
        all,
        AgentPickerResolution::Select(
            tau_proto::AgentId::parse("auto").expect("valid selected agent id")
        )
    );
}
