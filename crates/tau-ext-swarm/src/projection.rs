use std::collections::{BTreeMap, VecDeque};

use tau_swarm_api::{
    Agent, AgentId, BlockerId, BlockerPublication, BlockerRevisionNumber, SessionChange,
    SessionSnapshot, TaskId, TaskInfo, UpdatePublication, task_info_bytes,
};
use tau_swarm_client::{ChangeBatch, PublicationRevision, RevisionedSnapshot};
use tau_swarm_client_api::v0 as path_tau_swarm_client_api_v0;

/// Coherent, bounded in-memory projection of the current Tau session.
#[derive(Debug)]
pub(crate) struct SessionProjection {
    /// Current agents indexed by stable identifier.
    agents: BTreeMap<AgentId, Agent>,
    /// Active blocker revisions keyed by stable identifier.
    blockers: BTreeMap<BlockerId, BlockerPublication>,
    /// Immutable updates waiting for Swarm acknowledgement.
    updates: BTreeMap<tau_swarm_api::UpdateId, (PublicationRevision, UpdatePublication)>,
    /// Replaceable current task metadata keyed by exact task identity.
    task_info: BTreeMap<TaskId, TaskInfo>,
    /// Monotonic publication revision.
    revision: PublicationRevision,
    /// Retained changes paired with their resulting revision.
    changes: VecDeque<(PublicationRevision, SessionChange, usize)>,
    /// Maximum number of retained changes.
    capacity: usize,
    /// Maximum encoded bytes retained by changes.
    byte_capacity: usize,
    /// Current encoded retained-change bytes.
    change_bytes: usize,
    /// Maximum encoded current snapshot or individual change.
    publication_bytes: usize,
    /// Maximum current task metadata entries.
    task_info_capacity: usize,
}

impl SessionProjection {
    /// Creates an empty projection retaining at most `capacity` changes.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            agents: BTreeMap::new(),
            blockers: BTreeMap::new(),
            updates: BTreeMap::new(),
            task_info: BTreeMap::new(),
            revision: PublicationRevision(0),
            changes: VecDeque::new(),
            capacity,
            byte_capacity: usize::MAX,
            change_bytes: 0,
            publication_bytes: usize::MAX,
            task_info_capacity: tau_swarm_api::MAX_TASK_INFO_ENTRIES,
        }
    }

    /// Applies configured history and publication byte bounds.
    #[must_use]
    pub fn with_byte_limits(mut self, history_bytes: usize, publication_bytes: usize) -> Self {
        self.byte_capacity = history_bytes;
        self.publication_bytes = publication_bytes;
        self
    }

    /// Applies the configured local task-info entry ceiling.
    #[must_use]
    pub fn with_task_info_limit(mut self, task_info_entries: usize) -> Self {
        self.task_info_capacity = task_info_entries;
        self
    }

    /// Inserts or replaces an agent and publishes the replacement.
    pub fn upsert_agent(&mut self, agent: Agent) -> Result<(), &'static str> {
        let old = self.agents.insert(agent.id.clone(), agent.clone());
        let snapshot_length = self.snapshot_encoded_len();
        match old {
            Some(old) => {
                self.agents.insert(old.id.clone(), old);
            }
            None => {
                self.agents.remove(&agent.id);
            }
        }
        if self.publication_bytes < snapshot_length? {
            return Err("snapshot exceeds publication byte limit");
        }
        self.publish(SessionChange::UpsertAgent(agent.clone()))?;
        self.agents.insert(agent.id.clone(), agent);
        Ok(())
    }

    /// Removes an agent and publishes the removal when it existed.
    pub fn remove_agent(&mut self, id: &AgentId) -> Result<(), &'static str> {
        if self.agents.contains_key(id) {
            self.publish(SessionChange::RemoveAgent(id.clone()))?;
            self.agents.remove(id);
        }
        Ok(())
    }

    /// Returns whether the projection contains an agent.
    #[must_use]
    pub fn contains_agent(&self, id: &AgentId) -> bool {
        self.agents.contains_key(id)
    }

    /// Publishes a new active blocker revision.
    pub fn add_blocker(&mut self, blocker: BlockerPublication) -> Result<(), &'static str> {
        let old = self
            .blockers
            .insert(blocker.blocker_id.clone(), blocker.clone());
        let snapshot_length = self.snapshot_encoded_len();
        match old {
            Some(old) => {
                self.blockers.insert(old.blocker_id.clone(), old);
            }
            None => {
                self.blockers.remove(&blocker.blocker_id);
            }
        }
        if self.publication_bytes < snapshot_length? {
            return Err("snapshot exceeds publication byte limit");
        }
        self.publish(SessionChange::OpenBlocker(blocker.clone()))?;
        self.blockers.insert(blocker.blocker_id.clone(), blocker);
        Ok(())
    }

    /// Cancels an active blocker and publishes its removal.
    pub fn remove_blocker(
        &mut self,
        id: &BlockerId,
        reason: Option<String>,
    ) -> Result<(), &'static str> {
        if let Some(blocker) = self.blockers.get(id) {
            self.publish(SessionChange::CancelBlocker {
                blocker_id: id.clone(),
                revision: blocker.revision,
                reason,
            })?;
            self.blockers.remove(id);
        }
        Ok(())
    }

    /// Returns the exact active blocker publication.
    #[must_use]
    pub fn blocker(&self, id: &str, revision: u64) -> Option<BlockerPublication> {
        self.blockers
            .get(&BlockerId::new(id))
            .filter(|blocker| blocker.revision == BlockerRevisionNumber(revision))
            .cloned()
    }

    /// Removes an answered blocker from future active snapshots. The answer RPC
    /// itself carries the remote lifecycle transition, so this emits no cancel.
    pub fn close_answered_blocker(&mut self, id: &BlockerId) {
        self.blockers.remove(id);
    }

    /// Publishes and retains an immutable update until acknowledgement.
    pub fn add_update(&mut self, update: UpdatePublication) -> Result<(), &'static str> {
        if let Some((_, existing)) = self.updates.get(&update.id) {
            return if existing == &update {
                Ok(())
            } else {
                Err("update ID is already bound to a different payload")
            };
        }
        self.publish(SessionChange::AddUpdate(update.clone()))?;
        self.updates
            .insert(update.id.clone(), (self.revision, update));
        Ok(())
    }

    /// Returns stable unacknowledged updates at or before `revision`.
    #[must_use]
    pub fn pending_updates_through(&self, revision: PublicationRevision) -> Vec<UpdatePublication> {
        self.updates
            .values()
            .filter(|(created, _)| *created <= revision)
            .map(|(_, update)| update.clone())
            .collect()
    }

    /// Removes an acknowledged update from the process-memory outbox.
    pub fn acknowledge_update(&mut self, id: &tau_swarm_api::UpdateId) {
        self.updates.remove(id);
    }

    /// Returns retained update count and logical string bytes.
    #[must_use]
    pub fn update_usage(&self) -> (usize, usize) {
        let bytes = self
            .updates
            .values()
            .map(|(_, update)| {
                update.id.as_str().len()
                    + update.owner.as_str().len()
                    + update.title.len()
                    + update.description.len()
                    + update.task_id.as_ref().map_or(0, |id| id.as_str().len())
            })
            .sum();
        (self.updates.len(), bytes)
    }

    /// Atomically installs and publishes complete current task metadata.
    ///
    /// An exact replacement is a no-op. Admission and encoding checks complete
    /// before the history and map commit, so failure changes neither.
    pub fn upsert_task_info(&mut self, info: TaskInfo) -> Result<(), &'static str> {
        if self.task_info.get(&info.task_id) == Some(&info) {
            return Ok(());
        }
        let old = self.task_info.get(&info.task_id);
        if old.is_none() && self.task_info_capacity <= self.task_info.len() {
            return Err("task info entry limit is full");
        }
        task_info_bytes(
            self.task_info
                .values()
                .filter(|current| current.task_id != info.task_id)
                .chain(std::iter::once(&info)),
        )
        .map_err(|_| "task info byte limit is full")?;

        let old = self.task_info.insert(info.task_id.clone(), info.clone());
        let snapshot_length = self.snapshot_encoded_len();
        match old {
            Some(old) => {
                self.task_info.insert(old.task_id.clone(), old);
            }
            None => {
                self.task_info.remove(&info.task_id);
            }
        }
        if self.publication_bytes < snapshot_length? {
            return Err("snapshot exceeds publication byte limit");
        }
        self.publish(SessionChange::UpsertTaskInfo(info.clone()))?;
        self.task_info.insert(info.task_id.clone(), info);
        Ok(())
    }

    /// Returns a coherent snapshot and revision.
    #[must_use]
    pub fn snapshot(&self) -> RevisionedSnapshot {
        RevisionedSnapshot {
            revision: self.revision,
            snapshot: SessionSnapshot {
                agents: self.agents.values().cloned().collect(),
                active_blockers: self.blockers.values().cloned().collect(),
                task_info: self.task_info.values().cloned().collect(),
            },
        }
    }

    /// Returns retained changes after `revision`, or `None` when the caller
    /// fell behind.
    #[must_use]
    pub fn changes_after(&self, revision: PublicationRevision) -> Option<ChangeBatch> {
        let oldest = self
            .changes
            .front()
            .map_or(self.revision.0, |(revision, _, _)| {
                revision.0.saturating_sub(1)
            });
        if revision.0 < oldest {
            return None;
        }
        Some(ChangeBatch {
            revision: self.revision,
            changes: self
                .changes
                .iter()
                .filter(|(r, _, _)| revision < *r)
                .map(|(_, change, _)| change.clone())
                .collect(),
        })
    }

    fn publish(&mut self, change: SessionChange) -> Result<(), &'static str> {
        let encoded_bytes = change_encoded_len(change.clone())?;
        if self.publication_bytes < encoded_bytes {
            return Err("change exceeds publication byte limit");
        }
        let bytes = change_logical_bytes(&change)?;
        let revision = PublicationRevision(
            self.revision
                .0
                .checked_add(1)
                .ok_or("publication revision exhausted")?,
        );
        let change_bytes = self
            .change_bytes
            .checked_add(bytes)
            .ok_or("change byte accounting overflow")?;
        self.revision = revision;
        self.change_bytes = change_bytes;
        self.changes.push_back((revision, change, bytes));
        while self.capacity < self.changes.len() || self.byte_capacity < self.change_bytes {
            if let Some((_, _, bytes)) = self.changes.pop_front() {
                self.change_bytes = self.change_bytes.saturating_sub(bytes);
            }
        }
        Ok(())
    }

    fn snapshot_encoded_len(&self) -> Result<usize, &'static str> {
        let wire = tau_swarm_client_api::SubmitSnapshotRequest {
            snapshot: self.snapshot().snapshot.into(),
        };
        bincode::encode_to_vec(wire, tau_swarm_client_api::BINCODE_CONFIG)
            .map(|encoded| encoded.len())
            .map_err(|_| "snapshot encoding failed")
    }
}

fn change_logical_bytes(change: &SessionChange) -> Result<usize, &'static str> {
    let strings: Vec<&str> = match change {
        SessionChange::UpsertAgent(agent) => std::iter::once(agent.id.as_str())
            .chain(std::iter::once(agent.name.as_str()))
            .chain(agent.watches.iter().map(AgentId::as_str))
            .collect(),
        SessionChange::RemoveAgent(id) => vec![id.as_str()],
        SessionChange::OpenBlocker(blocker) => vec![
            Some(blocker.blocker_id.as_str()),
            Some(blocker.owner.as_str()),
            Some(blocker.title.as_str()),
            Some(blocker.description.as_str()),
            blocker.recommended_answer.as_deref(),
            blocker.task_id.as_ref().map(|id| id.as_str()),
        ]
        .into_iter()
        .flatten()
        .collect(),
        SessionChange::CancelBlocker {
            blocker_id, reason, ..
        } => std::iter::once(blocker_id.as_str())
            .chain(reason.as_deref())
            .collect(),
        SessionChange::AddUpdate(update) => vec![
            Some(update.id.as_str()),
            Some(update.owner.as_str()),
            Some(update.title.as_str()),
            Some(update.description.as_str()),
            update.task_id.as_ref().map(|id| id.as_str()),
        ]
        .into_iter()
        .flatten()
        .collect(),
        SessionChange::UpsertTaskInfo(info) => vec![
            info.task_id.as_str(),
            info.title.as_str(),
            info.description.as_ref().map_or("", |value| value.as_str()),
        ],
    };
    strings.into_iter().try_fold(0_usize, |total, value| {
        total
            .checked_add(value.len())
            .ok_or("change byte accounting overflow")
    })
}

fn change_encoded_len(change: SessionChange) -> Result<usize, &'static str> {
    let wire = tau_swarm_client_api::SubmitChangeRequest {
        sequence: 0,
        change: path_tau_swarm_client_api_v0::SessionChange::from(change),
    };
    bincode::encode_to_vec(wire, tau_swarm_client_api::BINCODE_CONFIG)
        .map(|encoded| encoded.len())
        .map_err(|_| "change encoding failed")
}

#[cfg(test)]
mod tests;
