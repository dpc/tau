use super::{AgentPickerResolution, resolve_agent_picker, with_agent_roster};

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
        work_status: Some(tau_proto::SessionAgentWorkStatus {
            phase: tau_proto::AgentWorkStatusPhase::Working,
            title: Some("shipping picker status".to_owned()),
        }),
    }
}

/// A running automatic agent forwards canonical status into picker rows before
/// fresh-snapshot revalidation; becoming idle preserves selection/draft in the
/// active picker, while the all-agent picker retains its category and selects
/// it.
#[test]
fn picker_orchestration_revalidates_with_initiating_category() {
    let running = auto_entry(tau_proto::AgentRuntimeState::Running);
    let idle = auto_entry(tau_proto::AgentRuntimeState::Idle);
    let pick_auto = |rows: &str| {
        assert!(rows.contains("auto\tlive\trunning\tactive_auto\t"));
        assert!(
            rows.lines()
                .all(|row| row.ends_with("\t$.00\tworking\tshipping picker status"))
        );
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

/// A timed-out current-harness roster request must surface its error before the
/// picker can launch, preserving the roster RPC as the sole initial deadline.
#[test]
fn picker_does_not_launch_after_current_roster_timeout() {
    let picker_launched = std::cell::Cell::new(false);
    let result = with_agent_roster(
        || Err("agent roster request timed out after 10s".to_owned()),
        |_| {
            picker_launched.set(true);
            AgentPickerResolution::NoChange
        },
    );

    assert_eq!(
        result,
        AgentPickerResolution::Notice("agent roster request timed out after 10s".to_owned())
    );
    assert!(!picker_launched.get());
}
