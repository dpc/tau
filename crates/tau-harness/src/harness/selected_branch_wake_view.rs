//! Owns one immutable projection of message wakes onto a selected agent branch.

use std::collections::{HashMap, HashSet, VecDeque};

use tau_core::{AgentTree, NodeId};

use crate::agent::{AgentMessageActivationClass, PendingMessageWake};

/// One selected branch and the message-wake facts projected onto it.
///
/// The scheduler builds this view once for the agent it selects. Readiness,
/// lifecycle classification, the earliest activation cut, and captured-cut
/// ancestry then share the same branch snapshot.
#[derive(Debug)]
pub(crate) struct SelectedBranchWakeView {
    /// Root-to-head node order for exact ancestry comparisons.
    branch: Vec<NodeId>,
    /// Parent of the earliest selected wake, when any wake is ready.
    earliest_activation_cut: Option<tau_proto::AgentHead>,
    /// Coalesced lifecycle class of every selected-branch wake.
    activation_class: Option<AgentMessageActivationClass>,
}

/// Exact allocation and traversal work for one streaming readiness probe.
pub(crate) struct SelectedBranchWakeProbe {
    /// Whether a materialized wake belongs to the selected branch.
    pub(crate) ready: bool,
    /// Pending wake records visited while building membership.
    pub(crate) wakes: usize,
    /// Selected-branch nodes visited before readiness settled.
    pub(crate) branch_nodes: usize,
    /// Owned lookup buffers allocated by the probe.
    pub(crate) owned_buffers: usize,
}

/// Exact construction work exposed to complexity regression tests.
#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct SelectedBranchWakeWork {
    /// Number of complete wake views constructed.
    pub(crate) view_builds: usize,
    /// Number of selected-branch nodes visited.
    pub(crate) branch_nodes: usize,
    /// Number of pending wakes visited.
    pub(crate) wakes: usize,
    /// Number of owned branch/wake lookup buffers allocated.
    pub(crate) owned_buffers: usize,
}

impl SelectedBranchWakeView {
    /// Probes wake readiness in linear work without materializing the branch.
    pub(crate) fn probe_ready(
        tree: &AgentTree,
        selected_head: Option<NodeId>,
        wakes: &VecDeque<PendingMessageWake>,
    ) -> SelectedBranchWakeProbe {
        let wake_nodes = wakes
            .iter()
            .filter_map(|wake| wake.node_id)
            .collect::<HashSet<_>>();
        let mut branch_nodes = 0;
        let mut cursor = selected_head;
        while let Some(node_id) = cursor {
            branch_nodes += 1;
            if wake_nodes.contains(&node_id) {
                return SelectedBranchWakeProbe {
                    ready: true,
                    wakes: wakes.len(),
                    branch_nodes,
                    owned_buffers: usize::from(!wake_nodes.is_empty()),
                };
            }
            cursor = tree.node(node_id).and_then(|node| node.parent_id);
        }
        SelectedBranchWakeProbe {
            ready: false,
            wakes: wakes.len(),
            branch_nodes,
            owned_buffers: usize::from(!wake_nodes.is_empty()),
        }
    }

    /// Projects the pending wake queue onto `selected_head` in linear work.
    pub(crate) fn new(
        tree: &AgentTree,
        selected_head: Option<NodeId>,
        wakes: &VecDeque<PendingMessageWake>,
    ) -> Self {
        Self::build(tree, selected_head, wakes, None)
    }

    /// Constructs one view and returns exact work for complexity tests.
    #[cfg(test)]
    pub(crate) fn new_measured(
        tree: &AgentTree,
        selected_head: Option<NodeId>,
        wakes: &VecDeque<PendingMessageWake>,
    ) -> (Self, SelectedBranchWakeWork) {
        let mut work = SelectedBranchWakeWork::default();
        let view = Self::build(tree, selected_head, wakes, Some(&mut work));
        (view, work)
    }

    /// Implements construction with optional exact work accounting.
    fn build(
        tree: &AgentTree,
        selected_head: Option<NodeId>,
        wakes: &VecDeque<PendingMessageWake>,
        mut work: Option<&mut SelectedBranchWakeWork>,
    ) -> Self {
        let branch = tree.branch_node_ids_from(selected_head);
        if let Some(work) = &mut work {
            work.view_builds = 1;
            work.branch_nodes = branch.len();
            work.wakes = wakes.len();
            work.owned_buffers = usize::from(!branch.is_empty()) + usize::from(!wakes.is_empty());
        }
        let mut wake_classes = HashMap::with_capacity(wakes.len());
        for wake in wakes {
            let Some(node_id) = wake.node_id else {
                continue;
            };
            let class = wake.source.activation_class();
            wake_classes
                .entry(node_id)
                .and_modify(|selected_class| {
                    if class == AgentMessageActivationClass::OrdinaryAgentInput {
                        *selected_class = class;
                    }
                })
                .or_insert(class);
        }

        let mut earliest_activation_cut = None;
        let mut activation_class = None;
        for node_id in &branch {
            let Some(class) = wake_classes.get(node_id).copied() else {
                continue;
            };
            if earliest_activation_cut.is_none() {
                earliest_activation_cut = tree.node(*node_id).map(|node| {
                    node.parent_id
                        .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node)
                });
            }
            activation_class = Some(match (activation_class, class) {
                (Some(AgentMessageActivationClass::OrdinaryAgentInput), _)
                | (_, AgentMessageActivationClass::OrdinaryAgentInput) => {
                    AgentMessageActivationClass::OrdinaryAgentInput
                }
                _ => AgentMessageActivationClass::IsolatedWatchNotification,
            });
        }

        Self {
            branch,
            earliest_activation_cut,
            activation_class,
        }
    }

    /// Returns whether at least one materialized wake is on the selected
    /// branch.
    pub(crate) fn has_ready_wake(&self) -> bool {
        self.activation_class.is_some()
    }

    /// Returns the coalesced lifecycle class of selected-branch wakes.
    pub(crate) fn activation_class(&self) -> Option<AgentMessageActivationClass> {
        self.activation_class
    }

    /// Merges a captured cut with the earliest selected message-wake cut.
    ///
    /// Both cuts must belong to the immutable selected branch. The older
    /// comparable cut wins so every owed activation remains in the suffix.
    pub(crate) fn earliest_activation_cut(
        &self,
        captured: Option<tau_proto::AgentHead>,
    ) -> Option<tau_proto::AgentHead> {
        match (captured, self.earliest_activation_cut) {
            (None, None) => None,
            (Some(captured), None) => self.contains(captured).then_some(captured),
            (None, Some(message)) => Some(message),
            (Some(captured), Some(message)) => {
                let captured_index = self.branch_index(captured)?;
                let message_index = self.branch_index(message)?;
                Some(if captured_index <= message_index {
                    captured
                } else {
                    message
                })
            }
        }
    }

    /// Returns whether `head` is root or a node in this selected branch.
    fn contains(&self, head: tau_proto::AgentHead) -> bool {
        self.branch_index(head).is_some()
    }

    /// Returns a root-inclusive position suitable for ancestry comparison.
    fn branch_index(&self, head: tau_proto::AgentHead) -> Option<usize> {
        match head {
            tau_proto::AgentHead::Root => Some(0),
            tau_proto::AgentHead::Node(node_id) => self
                .branch
                .iter()
                .position(|candidate| *candidate == node_id)
                .map(|index| index + 1),
        }
    }
}

#[cfg(test)]
mod tests;
