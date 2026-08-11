use std::cell as path_std_cell;

use super::{AgentPickerResolution, resolve_agent_picker, with_agent_roster};
use crate::estimated_cost::AgentCostSnapshot;
use crate::list_agents as path_crate_list_agents;

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
        work_status: Some(
            tau_proto::SessionAgentWorkStatus::new(
                tau_proto::AgentWorkStatusPhase::Working,
                Some("shipping picker status".to_owned()),
            )
            .expect("valid status"),
        ),
    }
}

/// Both picker paths project a creator's independent zero self estimate and
/// nonzero descendant estimate before fresh-snapshot revalidation; becoming
/// idle preserves selection/draft in the active picker, while the all-agent
/// picker retains its category and selects it.
#[test]
fn picker_orchestration_revalidates_with_initiating_category() {
    let running = auto_entry(tau_proto::AgentRuntimeState::Running);
    let idle = auto_entry(tau_proto::AgentRuntimeState::Idle);
    let mut other = running.clone();
    other.agent_id = tau_proto::AgentId::parse("other").expect("valid other agent id");
    let pick_auto = |rows: &str| {
        assert!(rows.contains("auto\tlive\trunning\tactive_auto\t"));
        assert!(
            rows.lines()
                .all(|row| row.ends_with("\t$.00/$2.1\t🚀\tshipping picker status\t💡"))
        );
        Ok(Some("auto\tlive\trunning\tactive_auto".to_owned()))
    };
    let creator_cost = AgentCostSnapshot::new(
        tau_proto::EstimatedApiCost::default(),
        tau_proto::EstimatedApiCost::from_picodollars(2_140_000_000_000),
    );

    let active = resolve_agent_picker(
        vec![running.clone(), other.clone()],
        path_crate_list_agents::AgentPickerFilter::Active,
        |_| Some(creator_cost),
        pick_auto,
        || Some(vec![idle.clone(), other.clone()]),
        || true,
        |agent_id| agent_id == "auto",
    );
    assert_eq!(
        active,
        AgentPickerResolution::Notice("selected agent is no longer available".to_owned())
    );

    let all = resolve_agent_picker(
        vec![running, other.clone()],
        path_crate_list_agents::AgentPickerFilter::All,
        |_| Some(creator_cost),
        pick_auto,
        || Some(vec![idle, other]),
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

/// Both picker categories launch the external picker for an empty roster, so
/// cancellation preserves the current agent instead of producing a local
/// notice.
#[test]
fn picker_launches_for_zero_candidates() {
    for filter in [
        path_crate_list_agents::AgentPickerFilter::Active,
        path_crate_list_agents::AgentPickerFilter::All,
    ] {
        let picker_launched = path_std_cell::Cell::new(false);
        let refresh_called = path_std_cell::Cell::new(false);
        let result = resolve_agent_picker(
            Vec::new(),
            filter,
            |_| None,
            |rows| {
                picker_launched.set(true);
                assert!(rows.is_empty());
                Ok(None)
            },
            || {
                refresh_called.set(true);
                Some(Vec::new())
            },
            || true,
            |_| false,
        );

        assert_eq!(result, AgentPickerResolution::NoChange);
        assert!(picker_launched.get());
        assert!(!refresh_called.get());
    }
}

/// Both picker categories launch the external picker for one candidate, so
/// cancellation never automatically switches to that candidate.
#[test]
fn picker_launches_for_one_candidate() {
    let running = auto_entry(tau_proto::AgentRuntimeState::Running);
    for filter in [
        path_crate_list_agents::AgentPickerFilter::Active,
        path_crate_list_agents::AgentPickerFilter::All,
    ] {
        let picker_launched = path_std_cell::Cell::new(false);
        let refresh_called = path_std_cell::Cell::new(false);
        let result = resolve_agent_picker(
            vec![running.clone()],
            filter,
            |_| None,
            |rows| {
                picker_launched.set(true);
                assert!(rows.starts_with("auto\tlive\trunning\tactive_auto\t"));
                Ok(None)
            },
            || {
                refresh_called.set(true);
                Some(vec![running.clone()])
            },
            || true,
            |agent_id| agent_id == "auto",
        );

        assert_eq!(result, AgentPickerResolution::NoChange);
        assert!(picker_launched.get());
        assert!(!refresh_called.get());
    }
}

/// A timed-out current-harness roster request must surface its error before the
/// picker can launch, preserving the roster RPC as the sole initial deadline.
#[test]
fn picker_does_not_launch_after_current_roster_timeout() {
    let picker_launched = path_std_cell::Cell::new(false);
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
