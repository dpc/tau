use std::os::unix::net as path_std_os_unix_net;
use std::{io as path_std_io, time as path_std_time};

use super::*;
use crate::estimated_cost::AgentCostSnapshot;

/// The direct roster request boundary must reject invalid controlled session
/// identifiers before attempting socket I/O.
#[test]
fn roster_request_rejects_invalid_session_id_without_panicking() {
    let error = run(&crate::cli::AgentListArgs {
        session_id: "bad.id".to_owned(),
        include_suspended: false,
        include_unavailable: false,
        include_unloaded: false,
        all: false,
    })
    .expect_err("invalid session id must fail");
    assert!(error.to_string().contains("invalid session id `bad.id`"));
}

fn entry(id: &str, parent: Option<&str>, started_at: Option<u64>) -> SessionAgentListEntry {
    SessionAgentListEntry {
        agent_id: tau_proto::AgentId::parse(id).expect("valid test id"),
        lifecycle: SessionAgentLifecycle::Live {
            runtime_state: tau_proto::AgentRuntimeState::Idle,
            navigation_mode: tau_proto::AgentNavigationMode::Active,
        },
        persistence: SessionAgentPersistence::Durable,
        facts: SessionAgentFacts::Available {
            started_at: started_at.map(tau_proto::UnixMicros::new),
            parent_agent: parent.map(|id| tau_proto::AgentId::parse(id).expect("valid parent id")),
            role: "engineer".to_owned(),
            display_name: None,
        },
        work_status: Some(tau_proto::SessionAgentWorkStatus::default()),
    }
}

/// Default visibility keeps live non-suspended rows, including idle
/// active-auto.
#[test]
fn default_filter_is_mode_not_suspended() {
    let mut active_auto = entry("auto", None, Some(1));
    active_auto.lifecycle = SessionAgentLifecycle::Live {
        runtime_state: tau_proto::AgentRuntimeState::Idle,
        navigation_mode: tau_proto::AgentNavigationMode::ActiveAuto,
    };
    let mut suspended = entry("suspended", None, Some(2));
    suspended.lifecycle = SessionAgentLifecycle::Live {
        runtime_state: tau_proto::AgentRuntimeState::Idle,
        navigation_mode: tau_proto::AgentNavigationMode::Suspended,
    };
    let mut unavailable = entry("unavailable", None, Some(3));
    unavailable.lifecycle = SessionAgentLifecycle::Unavailable;

    let visible = visible_agents(
        vec![suspended, unavailable, active_auto],
        AgentListFilter::default(),
    );

    assert_eq!(
        visible
            .into_iter()
            .map(|agent| agent.agent_id.to_string())
            .collect::<Vec<_>>(),
        vec!["auto"]
    );
}

/// The active picker follows effective navigation eligibility without treating
/// idle as globally suspended or running as globally active.
#[test]
fn active_picker_filters_navigation_and_runtime_state_independently() {
    let cases = [
        (
            "active-running",
            tau_proto::AgentNavigationMode::Active,
            tau_proto::AgentRuntimeState::Running,
            true,
        ),
        (
            "active-idle",
            tau_proto::AgentNavigationMode::Active,
            tau_proto::AgentRuntimeState::Idle,
            true,
        ),
        (
            "auto-running",
            tau_proto::AgentNavigationMode::ActiveAuto,
            tau_proto::AgentRuntimeState::Running,
            true,
        ),
        (
            "auto-idle",
            tau_proto::AgentNavigationMode::ActiveAuto,
            tau_proto::AgentRuntimeState::Idle,
            false,
        ),
        (
            "suspended-running",
            tau_proto::AgentNavigationMode::Suspended,
            tau_proto::AgentRuntimeState::Running,
            false,
        ),
        (
            "suspended-idle",
            tau_proto::AgentNavigationMode::Suspended,
            tau_proto::AgentRuntimeState::Idle,
            false,
        ),
    ];
    let rows = cases
        .iter()
        .enumerate()
        .map(|(index, (id, mode, runtime, _))| {
            let mut row = entry(id, None, Some(index as u64));
            row.lifecycle = SessionAgentLifecycle::Live {
                runtime_state: *runtime,
                navigation_mode: *mode,
            };
            row
        })
        .collect();

    let visible = picker_agents(rows, AgentPickerFilter::Active)
        .into_iter()
        .map(|agent| agent.agent_id.to_string())
        .collect::<std::collections::HashSet<_>>();

    for (id, _, _, expected) in cases {
        assert_eq!(visible.contains(id), expected, "{id}");
    }
}

/// The all-agent picker includes every live navigation mode while retaining
/// each row's independent running or idle output column.
#[test]
fn all_picker_includes_suspended_agents_and_preserves_runtime_column() {
    let mut running = entry("suspended-running", None, Some(1));
    running.lifecycle = SessionAgentLifecycle::Live {
        runtime_state: tau_proto::AgentRuntimeState::Running,
        navigation_mode: tau_proto::AgentNavigationMode::Suspended,
    };
    let mut idle = entry("auto-idle", None, Some(2));
    idle.lifecycle = SessionAgentLifecycle::Live {
        runtime_state: tau_proto::AgentRuntimeState::Idle,
        navigation_mode: tau_proto::AgentNavigationMode::ActiveAuto,
    };
    idle.facts = SessionAgentFacts::Missing;

    let output = format_rows(&picker_agents(vec![running, idle], AgentPickerFilter::All));

    assert!(output.contains("suspended-running\tlive\trunning\tsuspended\t"));
    assert!(output.contains("auto-idle\tlive\tidle\tactive_auto\tdurable\tmissing\t"));
}

/// Picker rows append canonical cost and status facts without changing public
/// roster fields or ordering.
#[test]
fn picker_rows_append_canonical_cost_and_status() {
    let zero = entry("zero", None, Some(1));
    let mut nonzero = entry("nonzero", None, Some(2));
    nonzero.work_status = Some(
        tau_proto::SessionAgentWorkStatus::new(
            tau_proto::AgentWorkStatusPhase::Working,
            Some("verify \\ picker\u{202e} rows".to_owned()),
        )
        .expect("valid status"),
    );
    let unavailable = entry("unavailable", None, Some(3));
    let output = format_picker_rows(&[zero, nonzero, unavailable], |agent_id| {
        match agent_id.as_str() {
            "zero" => Some(AgentCostSnapshot::new(
                tau_proto::EstimatedApiCost::default(),
                tau_proto::EstimatedApiCost::default(),
            )),
            "nonzero" => Some(AgentCostSnapshot::new(
                tau_proto::EstimatedApiCost::from_picodollars(2_140_000_000_000),
                tau_proto::EstimatedApiCost::from_picodollars(4_280_000_000_000),
            )),
            _ => None,
        }
    });
    let extras = output
        .lines()
        .map(|row| row.split('\t').skip(10).collect::<Vec<_>>())
        .collect::<Vec<_>>();
    assert_eq!(
        extras,
        [
            vec!["$.00/$.00", "unreported", "-"],
            vec!["$2.1/$4.3", "working", r"verify \\ picker\\u{202E} rows"],
            vec!["-/-", "unreported", "-"],
        ]
    );
}

/// Every closed canonical work-status phase maps to its stable picker spelling.
#[test]
fn picker_work_status_phase_names_are_complete() {
    use tau_proto::AgentWorkStatusPhase::{Blocked, Done, Unknown, Unreported, Working};

    assert_eq!(
        [Unreported, Working, Done, Blocked, Unknown].map(work_status_phase_name),
        ["unreported", "working", "done", "blocked", "unknown"]
    );
}

/// Picker membership uses live lifecycle authority even when independent
/// creation-fact enrichment is missing, invalid, or unreadable.
#[test]
fn pickers_keep_live_agents_without_available_creation_facts() {
    let mut missing = entry("missing", None, Some(1));
    missing.facts = SessionAgentFacts::Missing;
    let mut invalid = entry("invalid", None, Some(2));
    invalid.facts = SessionAgentFacts::Invalid;
    let mut unreadable = entry("unreadable", None, Some(3));
    unreadable.facts = SessionAgentFacts::Unreadable;
    let mut unavailable = entry("unavailable", None, Some(4));
    unavailable.lifecycle = SessionAgentLifecycle::Unavailable;
    let mut unloaded = entry("unloaded", None, Some(5));
    unloaded.lifecycle = SessionAgentLifecycle::Unloaded;

    for filter in [AgentPickerFilter::Active, AgentPickerFilter::All] {
        let visible = picker_agents(
            vec![
                missing.clone(),
                invalid.clone(),
                unreadable.clone(),
                unavailable.clone(),
                unloaded.clone(),
            ],
            filter,
        )
        .into_iter()
        .map(|agent| agent.agent_id.to_string())
        .collect::<Vec<_>>();
        assert_eq!(visible, vec!["invalid", "missing", "unreadable"]);
    }
}

/// Revalidation keeps the initiating picker category when an automatic agent
/// becomes idle between the displayed and fresh snapshots.
#[test]
fn picker_revalidation_preserves_active_or_all_category() {
    let mut running = entry("auto", None, Some(1));
    running.lifecycle = SessionAgentLifecycle::Live {
        runtime_state: tau_proto::AgentRuntimeState::Running,
        navigation_mode: tau_proto::AgentNavigationMode::ActiveAuto,
    };
    let mut idle = running.clone();
    idle.lifecycle = SessionAgentLifecycle::Live {
        runtime_state: tau_proto::AgentRuntimeState::Idle,
        navigation_mode: tau_proto::AgentNavigationMode::ActiveAuto,
    };

    let selected = &running.agent_id;
    assert!(picker_selection_is_current(
        &[running.clone()],
        selected,
        AgentPickerFilter::Active
    ));
    assert!(!picker_selection_is_current(
        &[idle.clone()],
        selected,
        AgentPickerFilter::Active
    ));
    assert!(picker_selection_is_current(
        &[idle],
        selected,
        AgentPickerFilter::All
    ));
}

/// The all-categories filter admits suspended, unavailable, unreadable, and
/// unloaded rows.
#[test]
fn all_category_filter_is_additive() {
    let mut suspended = entry("suspended", None, Some(1));
    suspended.lifecycle = SessionAgentLifecycle::Live {
        runtime_state: tau_proto::AgentRuntimeState::Idle,
        navigation_mode: tau_proto::AgentNavigationMode::Suspended,
    };
    let mut unavailable = entry("unavailable", None, Some(2));
    unavailable.lifecycle = SessionAgentLifecycle::Unavailable;
    unavailable.facts = SessionAgentFacts::Unreadable;
    let mut unloaded = entry("unloaded", None, Some(3));
    unloaded.lifecycle = SessionAgentLifecycle::Unloaded;

    let visible = visible_agents(
        vec![unloaded, unavailable, suspended],
        AgentListFilter {
            include_suspended: true,
            include_unavailable: true,
            include_unloaded: true,
        },
    );

    assert_eq!(visible.len(), 3);
}

/// Each public inclusion flag remains additive and `--all` maps to all three.
#[test]
fn public_filter_flags_map_independently_and_all_enables_every_flag() {
    let base = crate::cli::AgentListArgs {
        session_id: "s1".to_owned(),
        include_suspended: false,
        include_unavailable: false,
        include_unloaded: false,
        all: false,
    };
    for (expected, args) in [
        (
            AgentListFilter {
                include_suspended: true,
                ..AgentListFilter::default()
            },
            crate::cli::AgentListArgs {
                include_suspended: true,
                ..base.clone()
            },
        ),
        (
            AgentListFilter {
                include_unavailable: true,
                ..AgentListFilter::default()
            },
            crate::cli::AgentListArgs {
                include_unavailable: true,
                ..base.clone()
            },
        ),
        (
            AgentListFilter {
                include_unloaded: true,
                ..AgentListFilter::default()
            },
            crate::cli::AgentListArgs {
                include_unloaded: true,
                ..base.clone()
            },
        ),
    ] {
        assert_eq!(AgentListFilter::from_args(&args), expected);
    }
    assert_eq!(
        AgentListFilter::from_args(&crate::cli::AgentListArgs { all: true, ..base }),
        AgentListFilter {
            include_suspended: true,
            include_unavailable: true,
            include_unloaded: true,
        }
    );
}

/// Parent edges outrank creation timestamps while ready-node ties use timestamp
/// then stable agent id.
#[test]
fn topological_order_places_parent_before_child() {
    let rows = vec![
        entry("child", Some("parent"), Some(1)),
        entry("later-root", None, Some(3)),
        entry("parent", None, Some(9)),
        entry("early-root", None, Some(2)),
    ];

    let ordered = topological_order(rows)
        .into_iter()
        .map(|agent| agent.agent_id.to_string())
        .collect::<Vec<_>>();

    assert_eq!(ordered, vec!["early-root", "later-root", "parent", "child"]);
}

/// Malformed parent cycles are broken deterministically without dropping rows.
#[test]
fn topological_order_preserves_cycle_rows() {
    let rows = vec![
        entry("b", Some("a"), Some(2)),
        entry("a", Some("b"), Some(1)),
    ];

    let ordered = topological_order(rows)
        .into_iter()
        .map(|agent| agent.agent_id.to_string())
        .collect::<Vec<_>>();

    assert_eq!(ordered, vec!["a", "b"]);
}

/// Unknown timestamps, orphan/self parents, and input permutations keep one
/// deterministic total order.
#[test]
fn topological_order_is_stable_for_legacy_and_orphan_rows() {
    let rows = vec![
        entry("unknown-b", None, None),
        entry("orphan", Some("missing"), Some(2)),
        entry("self-parent", Some("self-parent"), Some(1)),
        entry("unknown-a", None, None),
    ];
    let expected = vec!["self-parent", "orphan", "unknown-a", "unknown-b"];
    for rows in [rows.clone(), rows.into_iter().rev().collect()] {
        assert_eq!(
            topological_order(rows)
                .into_iter()
                .map(|agent| agent.agent_id.to_string())
                .collect::<Vec<_>>(),
            expected
        );
    }
}

/// TSV output keeps the stable id in field one and escapes free-text controls.
#[test]
fn format_rows_escapes_free_text() {
    let mut row = entry("agent", None, Some(42));
    row.facts = SessionAgentFacts::Available {
        started_at: Some(tau_proto::UnixMicros::new(42)),
        parent_agent: None,
        role: "role\twith\\slash".to_owned(),
        display_name: Some("line\nname".to_owned()),
    };

    let output = format_rows(&[row]);
    let fields = output.trim_end().split('\t').collect::<Vec<_>>();

    assert_eq!(fields.len(), 10);
    assert_eq!(fields[0], "agent");
    assert_eq!(fields[6], "role\\twith\\\\slash");
    assert_eq!(fields[9], "line\\nname");
}

/// The stable schema renders representative sentinels and enum spellings in
/// exactly ten columns.
#[test]
fn format_rows_matches_exact_ten_column_contract() {
    let mut row = entry("agent", None, None);
    row.lifecycle = SessionAgentLifecycle::Unavailable;
    row.persistence = SessionAgentPersistence::Ephemeral;
    row.facts = SessionAgentFacts::Invalid;

    assert_eq!(
        format_rows(&[row]),
        "agent\tunavailable\t-\t-\tephemeral\tinvalid\t-\t-\t-\t-\n"
    );
}

/// Picker output accepts exactly one valid first-field agent id.
#[test]
fn selected_agent_id_rejects_multiline_or_invalid_rows() {
    assert_eq!(
        selected_agent_id("agent-1\tlive")
            .expect("valid selection")
            .as_str(),
        "agent-1"
    );
    assert!(selected_agent_id("bad/id\tlive").is_err());
    assert!(selected_agent_id("agent-1\nagent-2").is_err());
}

/// A connected but silent harness cannot wedge the one-shot roster request.
#[cfg(unix)]
#[test]
fn roster_request_times_out_on_silent_peer() {
    let temp = tempfile::tempdir().expect("tempdir");
    let socket_path = temp.path().join("harness.sock");
    let listener = path_std_os_unix_net::UnixListener::bind(&socket_path).expect("bind listener");
    let server = std::thread::spawn(move || {
        let (_stream, _) = listener.accept().expect("accept client");
        std::thread::sleep(Duration::from_millis(100));
    });

    let started = path_std_time::Instant::now();
    let result = request_at_socket_with_timeout_typed(
        &socket_path,
        &tau_proto::SessionId::parse("s1").expect("session id"),
        SessionAgentListScope::Current,
        Duration::from_millis(20),
    );

    assert!(result.is_err());
    assert!(started.elapsed() < Duration::from_secs(1));
    server.join().expect("server thread");
}

/// Unrelated directed frames do not reset the absolute one-shot RPC deadline.
#[cfg(unix)]
#[test]
fn roster_request_deadline_survives_unrelated_frames() {
    let temp = tempfile::tempdir().expect("tempdir");
    let socket_path = temp.path().join("harness.sock");
    let listener = path_std_os_unix_net::UnixListener::bind(&socket_path).expect("bind listener");
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept client");
        let mut writer = tau_proto::HarnessOutputWriter::new(path_std_io::BufWriter::new(stream));
        for index in 0..20 {
            if writer
                .write_message(&HarnessOutputMessage::PeerSessionProbeResult(
                    tau_proto::PeerSessionProbeResult {
                        request_id: format!("unrelated-{index}"),
                        available: false,
                    },
                ))
                .is_err()
            {
                break;
            }
            let _ = writer.flush();
            std::thread::sleep(Duration::from_millis(2));
        }
    });

    let started = path_std_time::Instant::now();
    let result = request_at_socket_with_timeout_typed(
        &socket_path,
        &tau_proto::SessionId::parse("s1").expect("session id"),
        SessionAgentListScope::Current,
        Duration::from_millis(20),
    );

    assert!(result.is_err());
    assert!(started.elapsed() < Duration::from_secs(1));
    server.join().expect("server thread");
}

/// Incremental bytes from one incomplete frame cannot defeat the absolute
/// decoding deadline.
#[cfg(unix)]
#[test]
fn roster_request_deadline_stops_partial_frame_trickle() {
    use std::io::Write as _;

    let temp = tempfile::tempdir().expect("tempdir");
    let socket_path = temp.path().join("harness.sock");
    let listener = path_std_os_unix_net::UnixListener::bind(&socket_path).expect("bind listener");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept client");
        let frame = tau_proto::encode_message_to_vec(
            &HarnessOutputMessage::PeerSessionProbeResult(tau_proto::PeerSessionProbeResult {
                request_id: "unrelated".to_owned(),
                available: false,
            }),
        )
        .expect("encode frame");
        for byte in frame {
            if stream.write_all(&[byte]).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    });

    let started = path_std_time::Instant::now();
    let result = request_at_socket_with_timeout_typed(
        &socket_path,
        &tau_proto::SessionId::parse("s1").expect("session id"),
        SessionAgentListScope::Current,
        Duration::from_millis(20),
    );

    assert!(result.is_err());
    assert!(started.elapsed() < Duration::from_secs(1));
    server.join().expect("server thread");
}

/// A saturated Unix listen backlog cannot hold connection establishment beyond
/// the same absolute RPC deadline.
#[cfg(target_os = "linux")]
#[test]
fn roster_request_deadline_bounds_saturated_backlog_connect() {
    let temp = tempfile::tempdir().expect("tempdir");
    let socket_path = temp.path().join("harness.sock");
    let listener = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)
        .expect("listener socket");
    listener
        .bind(&socket2::SockAddr::unix(&socket_path).expect("socket address"))
        .expect("bind listener");
    listener.listen(1).expect("listen");

    let mut backlog = Vec::new();
    for _ in 0..16 {
        let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)
            .expect("backlog socket");
        match socket.connect_timeout(
            &socket2::SockAddr::unix(&socket_path).expect("socket address"),
            Duration::from_millis(10),
        ) {
            Ok(()) => backlog.push(socket),
            Err(_) => break,
        }
    }
    assert!(!backlog.is_empty());

    let started = path_std_time::Instant::now();
    let result = request_at_socket_with_timeout_typed(
        &socket_path,
        &tau_proto::SessionId::parse("s1").expect("session id"),
        SessionAgentListScope::Current,
        Duration::from_millis(20),
    );

    assert!(result.is_err());
    assert!(started.elapsed() < Duration::from_secs(1));
}
