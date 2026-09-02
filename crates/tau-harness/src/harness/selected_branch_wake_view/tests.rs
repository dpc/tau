use std::collections::VecDeque;
use std::hint::black_box;
use std::time::Instant;

use tau_core::{AgentTree, PersistedAgentEventSeq};
use tau_proto::{AgentHead, AgentId, Event, PromptOriginator};

use super::*;
use crate::agent::{DeliveryDeadlineKind, PendingMessageWakeSource};

/// Appends one ordinary transcript node and returns its identifier.
fn append_node(tree: &mut AgentTree, agent_id: &AgentId, label: &str) -> NodeId {
    tree.apply_event(&Event::AgentPromptSubmitted(
        tau_proto::AgentPromptSubmitted {
            inference_activation: false,
            agent_id: agent_id.clone(),
            text: label.to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        },
    ));
    tree.head().expect("appended node")
}

/// Creates one payload-free wake for a materialized or deferred node.
fn wake(
    sequence: u64,
    node_id: Option<NodeId>,
    class: AgentMessageActivationClass,
) -> PendingMessageWake {
    PendingMessageWake {
        source: PendingMessageWakeSource::AgentMessageReceived {
            durable_event_seq: PersistedAgentEventSeq::new(sequence),
            activation_class: class,
            peer_admission_bytes: None,
        },
        node_id,
        activation_observation: None,
        source_observation: None,
        delivery_schedule: None,
    }
}

/// Fixed-seed random branches and wake queues keep the optimized projection
/// exactly equal to an allocating reference at every selectable head.
#[test]
fn randomized_view_matches_reference_at_every_head() {
    let agent_id = AgentId::parse("wake-view-agent").expect("agent id");
    let mut tree = AgentTree::from_events(agent_id.clone(), &[]);
    let mut heads = vec![AgentHead::Root];
    let mut nodes = Vec::new();
    let mut random = 0x6a09_e667_f3bc_c909_u64;
    for step in 0..72 {
        random = random
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        let parent = heads[random as usize % heads.len()];
        tree.apply_event(&Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: agent_id.clone(),
            head: parent,
        }));
        let node = append_node(&mut tree, &agent_id, &format!("node {step}"));
        nodes.push(node);
        heads.push(AgentHead::Node(node));
    }

    for (head_index, head) in heads.iter().copied().enumerate() {
        tree.apply_event(&Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: agent_id.clone(),
            head,
        }));
        let mut wakes = VecDeque::new();
        for (index, node) in nodes.iter().copied().enumerate() {
            random = random
                .wrapping_mul(2_862_933_555_777_941_757)
                .wrapping_add(3_037_000_493);
            if (random ^ head_index as u64).is_multiple_of(5) {
                let class = if random.is_multiple_of(3) {
                    AgentMessageActivationClass::OrdinaryAgentInput
                } else {
                    AgentMessageActivationClass::IsolatedWatchNotification
                };
                wakes.push_back(wake(index as u64, Some(node), class));
            }
            if random.is_multiple_of(11) {
                wakes.push_back(wake(
                    (nodes.len() + index) as u64,
                    None,
                    AgentMessageActivationClass::OrdinaryAgentInput,
                ));
            }
        }

        let branch = tree.branch_node_ids_from(head.as_option());
        let selected = branch
            .iter()
            .filter_map(|node| {
                let classes = wakes
                    .iter()
                    .filter(|wake| wake.node_id == Some(*node))
                    .map(|wake| wake.source.activation_class())
                    .collect::<Vec<_>>();
                (!classes.is_empty()).then_some((*node, classes))
            })
            .collect::<Vec<_>>();
        let expected_class = (!selected.is_empty()).then(|| {
            if selected
                .iter()
                .flat_map(|(_, classes)| classes)
                .all(|class| *class == AgentMessageActivationClass::IsolatedWatchNotification)
            {
                AgentMessageActivationClass::IsolatedWatchNotification
            } else {
                AgentMessageActivationClass::OrdinaryAgentInput
            }
        });
        let expected_cut = selected.first().and_then(|(node, _)| {
            tree.node(*node)
                .map(|node| node.parent_id.map_or(AgentHead::Root, AgentHead::Node))
        });
        let view = SelectedBranchWakeView::new(&tree, head.as_option(), &wakes);
        let probe = SelectedBranchWakeView::probe_ready(
            &tree,
            head.as_option(),
            &wakes,
            DeliveryDeadlineKind::Idle,
        );
        assert_eq!(probe.ready, expected_class.is_some());
        assert_eq!(view.has_ready_wake(), expected_class.is_some());
        assert_eq!(view.activation_class(), expected_class);
        assert_eq!(view.earliest_activation_cut(None), expected_cut);

        for captured in heads
            .iter()
            .copied()
            .chain([AgentHead::Node(NodeId::new(u64::MAX))])
        {
            let captured_index = match captured {
                AgentHead::Root => Some(0),
                AgentHead::Node(node) => branch
                    .iter()
                    .position(|candidate| *candidate == node)
                    .map(|i| i + 1),
            };
            let message_index = expected_cut.and_then(|cut| match cut {
                AgentHead::Root => Some(0),
                AgentHead::Node(node) => branch
                    .iter()
                    .position(|candidate| *candidate == node)
                    .map(|i| i + 1),
            });
            let expected = match (captured_index, message_index) {
                (None, _) => None,
                (Some(_), None) => Some(captured),
                (Some(captured_index), Some(message_index)) => {
                    Some(if captured_index <= message_index {
                        captured
                    } else {
                        expected_cut.expect("message cut")
                    })
                }
            };
            assert_eq!(view.earliest_activation_cut(Some(captured)), expected);
        }
    }
}

/// Exact work counters pin one view and one linear visit of branch and wake
/// inputs rather than repeated readiness/class/cut projections.
#[test]
fn measured_view_visits_branch_and_wakes_once() {
    let agent_id = AgentId::parse("wake-work-agent").expect("agent id");
    let mut tree = AgentTree::from_events(agent_id.clone(), &[]);
    let nodes = (0..128)
        .map(|index| append_node(&mut tree, &agent_id, &format!("node {index}")))
        .collect::<Vec<_>>();
    let wakes = nodes
        .iter()
        .step_by(3)
        .enumerate()
        .map(|(index, node)| {
            wake(
                index as u64,
                Some(*node),
                AgentMessageActivationClass::OrdinaryAgentInput,
            )
        })
        .collect::<VecDeque<_>>();
    let (_, work) = SelectedBranchWakeView::new_measured(&tree, tree.head(), &wakes);
    let probe =
        SelectedBranchWakeView::probe_ready(&tree, tree.head(), &wakes, DeliveryDeadlineKind::Idle);
    assert_eq!(
        work,
        SelectedBranchWakeWork {
            view_builds: 1,
            branch_nodes: nodes.len(),
            wakes: wakes.len(),
            owned_buffers: 2,
        }
    );
    assert!(probe.ready);
    assert_eq!(probe.wakes, wakes.len());
    assert!(probe.branch_nodes <= nodes.len());
    assert_eq!(probe.owned_buffers, 1);
}

/// Prints descriptive release timings by agent count, branch depth, and
/// wake count. This benchmark has no pass/fail timing threshold.
#[test]
#[ignore = "manual selected-branch wake-view scaling benchmark"]
fn benchmark_selected_branch_wake_view_scaling() {
    println!("agents,depth,wakes,iterations,elapsed_ns");
    for agents in [1, 16, 64] {
        for depth in [16, 64, 256, 1_024] {
            let agent_id = AgentId::parse(format!("wake-bench-{depth}")).expect("agent id");
            let mut tree = AgentTree::from_events(agent_id.clone(), &[]);
            let nodes = (0..depth)
                .map(|index| append_node(&mut tree, &agent_id, &index.to_string()))
                .collect::<Vec<_>>();
            for wake_count in [0, depth / 4, depth] {
                let wakes = (0..wake_count)
                    .map(|index| {
                        wake(
                            index as u64,
                            Some(nodes[index % nodes.len()]),
                            AgentMessageActivationClass::OrdinaryAgentInput,
                        )
                    })
                    .collect::<VecDeque<_>>();
                let iterations = 1_000;
                let started = Instant::now();
                for _ in 0..iterations {
                    for _ in 0..agents {
                        black_box(SelectedBranchWakeView::probe_ready(
                            black_box(&tree),
                            tree.head(),
                            black_box(&wakes),
                            DeliveryDeadlineKind::Idle,
                        ));
                    }
                    black_box(SelectedBranchWakeView::new(
                        black_box(&tree),
                        tree.head(),
                        black_box(&wakes),
                    ));
                }
                println!(
                    "{agents},{depth},{wake_count},{iterations},{}",
                    started.elapsed().as_nanos()
                );
            }
        }
    }
}
