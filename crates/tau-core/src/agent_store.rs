//! Per-agent protocol event storage.
//!
//! Durable agents are CBOR event logs plus small JSON sidecars. Ephemeral
//! agents use the same in-memory [`AgentTree`] and replay event records, but
//! those records live only inside the currently running store. Writers go
//! through [`AgentStore::append_agent_event`], which applies each accepted
//! transcript event to the cached tree after persistence. Raw message facts use
//! [`AgentStore::append_agent_message_fact_at`] so their canonical append
//! precedes and cannot be vetoed by post-commit projection.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use tau_proto::{
    AgentId, AgentIdParseError, ConnectionId, Event, EventName, MessageAgentTarget, NodeId,
    UnixMicros,
};

use crate::agent_checkpoint::{
    AgentCheckpoint, AgentSummary, CommittedJournalPosition, journal_position, read_checkpoint,
    read_journal_bound_checkpoint, write_checkpoint_atomic,
};
use crate::record_log::MAX_RECORD_BYTES;
use crate::session::{
    AgentEventParent, AgentEventValidationError, AgentMeta, AgentTree, PersistedAgentEvent,
    PersistedAgentEventSeq,
};

/// Errors returned by the append-only agent store.
#[derive(Debug)]
pub enum AgentStoreError {
    CreateParentDirectory {
        path: PathBuf,
        source: io::Error,
    },
    Open {
        path: PathBuf,
        source: io::Error,
    },
    Read {
        path: PathBuf,
        source: io::Error,
    },
    Write {
        path: PathBuf,
        source: io::Error,
    },
    Decode {
        path: PathBuf,
        source: tau_proto::DecodeError,
    },
    Encode {
        path: PathBuf,
        source: tau_proto::EncodeError,
    },
    /// Encoded record exceeded the loader's matching allocation bound.
    RecordTooLarge {
        path: PathBuf,
        record_length: u64,
        maximum: u64,
    },
    /// Another process holds the exclusive lock on this agent.
    Locked {
        path: PathBuf,
        holder: String,
    },
    InvalidAgentDir {
        path: PathBuf,
    },
    InvalidAgentId {
        agent_id: String,
        source: AgentIdParseError,
    },
    InvalidEvent {
        source: AgentEventValidationError,
    },
    /// A caller attempted raw append for an event outside the message category.
    UnsupportedRawEvent {
        /// Event name rejected by the raw message-fact append API.
        event_name: EventName,
    },
    /// A raw message fact claimed a target other than its selected journal.
    MessageFactTargetMismatch {
        /// Agent journal selected by the append caller.
        journal_agent_id: AgentId,
        /// Raw target carried by the fact, if the category supplied one.
        claimed_agent_id: Option<MessageAgentTarget>,
    },
    InvalidSequence {
        path: PathBuf,
        expected: PersistedAgentEventSeq,
        actual: PersistedAgentEventSeq,
    },
    /// Requested memory-only storage for an id already reserved on disk.
    PersistenceConflict {
        agent_id: AgentId,
        path: PathBuf,
    },
}

#[cfg(test)]
mod tests;

impl fmt::Display for AgentStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateParentDirectory { path, source } => write!(
                f,
                "failed to create parent directory for agent store {}: {source}",
                path.display()
            ),
            Self::Open { path, source } => {
                write!(f, "failed to open agent store {}: {source}", path.display())
            }
            Self::Read { path, source } => {
                write!(f, "failed to read agent store {}: {source}", path.display())
            }
            Self::Write { path, source } => {
                write!(
                    f,
                    "failed to write agent store {}: {source}",
                    path.display()
                )
            }
            Self::Decode { path, source } => write!(
                f,
                "failed to decode agent store record from {}: {source}",
                path.display()
            ),
            Self::Encode { path, source } => write!(
                f,
                "failed to encode agent store record for {}: {source}",
                path.display()
            ),
            Self::RecordTooLarge {
                path,
                record_length,
                maximum,
            } => write!(
                f,
                "agent store record for {} is {record_length} bytes; maximum is {maximum}",
                path.display()
            ),
            Self::Locked { path, holder } => write!(
                f,
                "agent lock at {} held by another process ({})",
                path.display(),
                holder.trim()
            ),
            Self::InvalidAgentDir { path } => write!(
                f,
                "invalid agent directory name (non-utf8): {}",
                path.display()
            ),
            Self::InvalidAgentId { agent_id, source } => {
                write!(f, "invalid agent id `{agent_id}`: {source}")
            }
            Self::InvalidEvent { source } => write!(f, "invalid agent event: {source}"),
            Self::UnsupportedRawEvent { event_name } => {
                write!(f, "raw append only accepts message facts, got {event_name}")
            }
            Self::MessageFactTargetMismatch {
                journal_agent_id,
                claimed_agent_id,
            } => write!(
                f,
                "message fact target `{}` does not match agent journal `{journal_agent_id}`",
                claimed_agent_id
                    .as_ref()
                    .map_or("<missing>", MessageAgentTarget::as_str)
            ),
            Self::InvalidSequence {
                path,
                expected,
                actual,
            } => write!(
                f,
                "invalid agent event sequence in {}: expected {expected}, got {actual}",
                path.display()
            ),
            Self::PersistenceConflict { agent_id, path } => write!(
                f,
                "cannot mark agent `{agent_id}` ephemeral because durable state already exists at {}",
                path.display()
            ),
        }
    }
}

impl Error for AgentStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CreateParentDirectory { source, .. } => Some(source),
            Self::Open { source, .. } => Some(source),
            Self::Read { source, .. } => Some(source),
            Self::Write { source, .. } => Some(source),
            Self::Decode { source, .. } => Some(source),
            Self::Encode { source, .. } => Some(source),
            Self::InvalidAgentId { source, .. } => Some(source),
            Self::InvalidEvent { source } => Some(source),
            Self::RecordTooLarge { .. }
            | Self::Locked { .. }
            | Self::InvalidAgentDir { .. }
            | Self::InvalidSequence { .. }
            | Self::PersistenceConflict { .. }
            | Self::UnsupportedRawEvent { .. }
            | Self::MessageFactTargetMismatch { .. } => None,
        }
    }
}

/// Persistence policy for one agent transcript.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AgentPersistenceMode {
    /// Write transcript events, metadata, and locks under the agents root.
    #[default]
    Durable,
    /// Keep transcript events and metadata in memory for this process only.
    Ephemeral,
}

impl AgentPersistenceMode {
    /// Returns true when the agent transcript should be written to disk.
    #[must_use]
    pub const fn is_durable(self) -> bool {
        matches!(self, Self::Durable)
    }

    /// Returns true when the agent transcript should stay memory-only.
    #[must_use]
    pub const fn is_ephemeral(self) -> bool {
        matches!(self, Self::Ephemeral)
    }
}

/// Content-minimized facts read without replaying an agent transcript.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentCreationFacts {
    /// The first record is a valid matching `agent.started` fact.
    Available {
        /// Timestamp attached to the first record, unless legacy zero.
        started_at: Option<UnixMicros>,
        /// Parent copied from the immutable creation fact.
        parent_agent: Option<AgentId>,
        /// Role copied from the immutable creation fact.
        role: String,
        /// Display name from current memory or a journal-bound checkpoint.
        display_name: Option<String>,
        /// Bounded projection bytes consumed.
        bytes_read: u64,
    },
    /// The journal or its first record does not exist.
    Missing,
    /// The decoded first record is not a valid matching creation fact.
    Invalid {
        /// Bounded projection bytes consumed.
        bytes_read: u64,
    },
    /// I/O, decoding, or the individual record bound prevented classification.
    Unreadable {
        /// Bounded projection bytes consumed.
        bytes_read: u64,
    },
}

impl AgentCreationFacts {
    /// Returns the aggregate enrichment charge for this projection.
    #[must_use]
    pub const fn bytes_read(&self) -> u64 {
        match self {
            Self::Available { bytes_read, .. }
            | Self::Invalid { bytes_read }
            | Self::Unreadable { bytes_read } => *bytes_read,
            Self::Missing => 0,
        }
    }
}

/// Aggregate roster-enrichment budget was too small for the next first record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AgentCreationFactsBudgetExceeded;

impl fmt::Display for AgentCreationFactsBudgetExceeded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("agent creation-fact enrichment budget exceeded")
    }
}

impl Error for AgentCreationFactsBudgetExceeded {}

/// Caller-selected bounds for one shallow creation-fact projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AgentCreationFactsBudget {
    /// Largest encoded first record accepted.
    pub max_record_bytes: u64,
    /// Aggregate projection bytes still available to the caller.
    pub remaining_bytes: u64,
}

fn parse_agent_id_for_store(agent_id: &str) -> Result<AgentId, AgentStoreError> {
    AgentId::parse(agent_id).map_err(|source| AgentStoreError::InvalidAgentId {
        agent_id: agent_id.to_owned(),
        source,
    })
}

/// Result of one [`AgentStore::append_agent_event_at`] call:
/// the agent-event sequence and, when the event produced a tree node,
/// that node's id. Callers maintaining a per-conversation branch
/// cursor advance it from `folded_node_id` rather than from the
/// global `tree.head()` so side-state events and context projections deferred
/// behind an applicable tool round do not sync the cursor onto another branch's
/// last fold.
#[derive(Clone, Debug)]
pub struct AgentAppendOutcome {
    /// Sequence assigned to the record in this agent's event stream.
    pub seq: PersistedAgentEventSeq,
    /// Last tree node produced by this event, if any. A tool terminal that
    /// closes a round reports the last drained deferred-context node; an
    /// accepted context input deferred behind that round reports `None`
    /// until closure.
    pub folded_node_id: Option<NodeId>,
}

/// Agent protocol event store with a derived [`AgentTree`] cached in memory.
///
/// Each durable agent lives in its own directory under `agents_dir` (the
/// per-agent subdirectory of `state_dir`, typically
/// `<state_dir>/agents/`):
///
/// ```text
/// <agents_dir>/<agent_id>/
///   events.cbor   # length-prefixed PersistedAgentEvent stream — the source of truth
///   meta.json     # AgentMeta sidecar (cwd, created_at, last_touched)
///   lock          # exclusively flock'd while this store has the agent loaded for write
/// ```
///
/// Ephemeral agents are explicitly marked before their first write and keep the
/// same event stream, metadata, and folded tree in memory only. Existing
/// durable agent dirs are loaded lazily. Startup constructs an empty store and
/// loads individual agent trees on first access. Flocks are still taken lazily
/// on first durable write so read-only consumers (e.g. inspection commands)
/// don't contend with a running daemon.
///
/// Durable replay and memory-only parity follow
/// `DECISION-tau-core-semantic-store-durability`.
#[derive(Debug)]
pub struct AgentStore {
    agents_dir: PathBuf,
    agents: HashMap<AgentId, AgentTree>,
    /// Agents whose validated stream begins with their immutable creation fact.
    created_agents: HashSet<AgentId>,
    /// Memory-only agent ids owned by this process.
    ephemeral_agents: HashSet<AgentId>,
    /// Replay records for memory-only agents.
    ephemeral_events: HashMap<AgentId, Vec<PersistedAgentEvent>>,
    /// Sidecar metadata for memory-only agents.
    ephemeral_meta: HashMap<AgentId, AgentMeta>,
    /// Journal-derived summaries retained alongside loaded durable trees.
    summaries: HashMap<AgentId, AgentSummary>,
    /// Agents whose latest derived checkpoint could not be atomically
    /// published.
    dirty_checkpoints: HashSet<AgentId>,
    /// Held flocks per agent, acquired lazily on first write. Released
    /// when this store is dropped (the OS releases the flock when the
    /// file handle closes).
    locks: HashMap<AgentId, File>,
}

impl AgentStore {
    /// Opens the agent store rooted at `agents_dir`, eagerly loading
    /// every agent subdirectory found there.
    ///
    /// Cost is O(total bytes across every agent's `events.cbor`),
    /// so this is intended for read-only inspection callers (e.g.
    /// `tau agent list`) that genuinely need every tree resident in
    /// memory. Daemon startup should use [`Self::open_lazy`] and
    /// load individual trees on demand via [`Self::load_agent`].
    pub fn open(agents_dir: impl Into<PathBuf>) -> Result<Self, AgentStoreError> {
        let agents_dir = agents_dir.into();
        let mut store = Self::open_lazy(agents_dir.clone())?;
        for entry in fs::read_dir(&agents_dir).map_err(|source| AgentStoreError::Read {
            path: agents_dir.clone(),
            source,
        })? {
            let entry = entry.map_err(|source| AgentStoreError::Read {
                path: agents_dir.clone(),
                source,
            })?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let events_path = path.join("events.cbor");
            if !events_path.exists() {
                continue;
            }
            let agent_id_str = path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| AgentStoreError::InvalidAgentDir { path: path.clone() })?;
            store.load_agent_if_needed(agent_id_str)?;
        }
        Ok(store)
    }

    /// Opens the agent store rooted at `agents_dir` without
    /// loading agent event logs. Individual agents are loaded on
    /// write; callers that need a pre-existing tree should use
    /// [`Self::open`].
    pub fn open_lazy(agents_dir: impl Into<PathBuf>) -> Result<Self, AgentStoreError> {
        let agents_dir = agents_dir.into();
        fs::create_dir_all(&agents_dir).map_err(|source| {
            AgentStoreError::CreateParentDirectory {
                path: agents_dir.clone(),
                source,
            }
        })?;

        Ok(Self {
            agents_dir,
            agents: HashMap::new(),
            created_agents: HashSet::new(),
            ephemeral_agents: HashSet::new(),
            ephemeral_events: HashMap::new(),
            ephemeral_meta: HashMap::new(),
            summaries: HashMap::new(),
            dirty_checkpoints: HashSet::new(),
            locks: HashMap::new(),
        })
    }

    fn load_agent_if_needed(&mut self, agent_id: &str) -> Result<(), AgentStoreError> {
        let aid = parse_agent_id_for_store(agent_id)?;
        if self.agents.contains_key(&aid) {
            return Ok(());
        }
        if self.ephemeral_agents.contains(&aid) {
            return Ok(());
        }
        let events_path = self.agent_dir(agent_id).join("events.cbor");
        if !events_path.exists() {
            return Ok(());
        }
        // A temporary nonblocking lock lets an ordinary strict load migrate a
        // stable legacy/missing checkpoint without contending with a daemon.
        // Writers already retain the same lock in `self.locks`.
        let temporary_lock = if self.locks.contains_key(&aid) {
            None
        } else {
            let lock_path = self.agent_dir(agent_id).join("lock");
            let mut options = OpenOptions::new();
            options.create(true).read(true).write(true).truncate(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options
                .open(lock_path)
                .ok()
                .filter(|file| FileExt::try_lock_exclusive(file).is_ok())
        };
        let events = load_agent_events(&events_path)?;
        let tree = AgentTree::try_from_events(aid.clone(), &events)
            .map_err(|source| AgentStoreError::InvalidEvent { source })?;
        if records_begin_with_creation(&aid, &events) {
            self.created_agents.insert(aid.clone());
        }
        let mut summary = AgentSummary::default();
        for record in &events {
            summary.apply(record);
        }
        if records_begin_with_creation(&aid, &events)
            && (self.locks.contains_key(&aid) || temporary_lock.is_some())
        {
            let migration = (|| -> io::Result<()> {
                let mut journal = File::open(&events_path)?;
                let position = journal_position(&mut journal)?;
                let checkpoint = AgentCheckpoint::new(
                    aid.clone(),
                    summary.clone(),
                    PersistedAgentEventSeq::new(events.len() as u64),
                    &position,
                );
                write_checkpoint_atomic(&self.agent_dir(agent_id).join("meta.json"), &checkpoint)
            })();
            if let Err(error) = migration {
                self.dirty_checkpoints.insert(aid.clone());
                eprintln!(
                    "tau: agent checkpoint migration failed for {}: {error}",
                    aid.as_str()
                );
            } else {
                self.dirty_checkpoints.remove(&aid);
            }
        }
        self.summaries.insert(aid.clone(), summary);
        self.agents.insert(aid, tree);
        Ok(())
    }

    /// Returns the path to one agent's directory (created lazily on
    /// write).
    fn agent_dir(&self, agent_id: &str) -> PathBuf {
        self.agents_dir.join(agent_id)
    }

    /// Returns whether an agent already exists in memory or on disk.
    ///
    /// A durable `events.cbor` log or `meta.json` sidecar reserves the
    /// id even when this lazy store has not loaded that agent yet.
    #[must_use]
    pub fn agent_id_is_reserved(&self, agent_id: &str) -> bool {
        let Ok(aid) = AgentId::parse(agent_id) else {
            return false;
        };
        if self.agents.contains_key(&aid) {
            return true;
        }
        if self.ephemeral_agents.contains(&aid) {
            return true;
        }
        self.agent_dir(agent_id).exists()
    }

    /// Returns whether an agent has a loaded or journal-backed semantic
    /// identity.
    ///
    /// A `meta.json`-only artifact deliberately does not satisfy this routing
    /// predicate.
    #[must_use]
    pub fn agent_is_known_for_routing(&self, agent_id: &str) -> bool {
        let Ok(aid) = AgentId::parse(agent_id) else {
            return false;
        };
        self.agent_has_committed_identity(&aid)
    }

    /// Returns whether strict history starts with the agent's creation fact.
    ///
    /// Empty cached trees and zero-length journal artifacts are deliberately
    /// excluded: they reserve an id but cannot establish routing identity.
    #[must_use]
    pub fn agent_has_committed_identity(&self, agent_id: &AgentId) -> bool {
        if self.created_agents.contains(agent_id) {
            return true;
        }
        if let Some(events) = self.ephemeral_events.get(agent_id) {
            return records_begin_with_creation(agent_id, events);
        }
        let path = self.agent_dir(agent_id.as_str()).join("events.cbor");
        let Ok(events) = load_agent_events(&path) else {
            return false;
        };
        records_begin_with_creation(agent_id, &events)
            && AgentTree::try_from_events(agent_id.clone(), &events).is_ok()
    }

    /// Compatibility alias for conservative id reservation.
    #[must_use]
    pub fn agent_exists(&self, agent_id: &str) -> bool {
        self.agent_id_is_reserved(agent_id)
    }

    /// Marks an agent id as memory-only before its first transcript write.
    ///
    /// The id must not already be reserved by durable events or metadata. Once
    /// marked, all future event and metadata operations for this id stay
    /// process-local and [`Self::agent_events`] returns the in-memory replay
    /// stream.
    pub fn mark_agent_ephemeral(&mut self, agent_id: &str) -> Result<(), AgentStoreError> {
        let aid = parse_agent_id_for_store(agent_id)?;
        let agent_dir = self.agent_dir(agent_id);
        if agent_dir.exists() {
            return Err(AgentStoreError::PersistenceConflict {
                agent_id: aid,
                path: agent_dir,
            });
        }
        self.ephemeral_agents.insert(aid);
        Ok(())
    }

    /// Returns the persistence policy currently known for `agent_id`.
    #[must_use]
    pub fn agent_persistence(&self, agent_id: &str) -> AgentPersistenceMode {
        let Ok(agent_id) = AgentId::parse(agent_id) else {
            return AgentPersistenceMode::Durable;
        };
        if self.ephemeral_agents.contains(&agent_id) {
            AgentPersistenceMode::Ephemeral
        } else {
            AgentPersistenceMode::Durable
        }
    }

    /// Reads one bounded creation fact and current display-name projection.
    ///
    /// This method never scans beyond the first journal record, repairs a
    /// checkpoint, acquires a writer lock, or mutates the store.
    ///
    /// # Errors
    ///
    /// Returns [`AgentCreationFactsBudgetExceeded`] when a first record fits
    /// the individual record limit but exceeds `remaining_bytes`.
    pub fn agent_creation_facts(
        &self,
        agent_id: &AgentId,
        budget: AgentCreationFactsBudget,
    ) -> Result<AgentCreationFacts, AgentCreationFactsBudgetExceeded> {
        let AgentCreationFactsBudget {
            max_record_bytes,
            remaining_bytes,
        } = budget;
        if self.ephemeral_agents.contains(agent_id) {
            let Some(record) = self
                .ephemeral_events
                .get(agent_id)
                .and_then(|events| events.first())
            else {
                return Ok(AgentCreationFacts::Missing);
            };
            let display_name = self.agents.get(agent_id).and_then(AgentTree::display_name);
            let Some(record_bytes) = encoded_size_with_limit(record, max_record_bytes) else {
                return Ok(AgentCreationFacts::Unreadable { bytes_read: 0 });
            };
            let projected_bytes = record_bytes
                .saturating_add(display_name.map_or(0, |display_name| display_name.len() as u64));
            if projected_bytes > remaining_bytes {
                return Err(AgentCreationFactsBudgetExceeded);
            }
            return Ok(agent_creation_facts_from_record(
                agent_id,
                record,
                display_name.map(str::to_owned),
                projected_bytes,
            ));
        }

        let path = self.agent_dir(agent_id.as_str()).join("events.cbor");
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(AgentCreationFacts::Missing);
            }
            Err(_) => {
                return Ok(AgentCreationFacts::Unreadable { bytes_read: 0 });
            }
        };
        let record_length = match crate::record_log::read_record_length(&mut file) {
            Ok(Some(length)) => length,
            Ok(None) => {
                return Ok(AgentCreationFacts::Missing);
            }
            Err(_) => {
                return Ok(AgentCreationFacts::Unreadable { bytes_read: 0 });
            }
        };
        if record_length > max_record_bytes {
            return Ok(AgentCreationFacts::Unreadable { bytes_read: 0 });
        }
        if record_length > remaining_bytes {
            return Err(AgentCreationFactsBudgetExceeded);
        }
        let mut bytes = vec![0; record_length as usize];
        if file.read_exact(&mut bytes).is_err() {
            return Ok(AgentCreationFacts::Unreadable {
                bytes_read: record_length,
            });
        }
        let record = match tau_proto::decode_message_from_slice::<PersistedAgentEvent>(&bytes) {
            Ok(record) => record,
            Err(_) => {
                return Ok(AgentCreationFacts::Unreadable {
                    bytes_read: record_length,
                });
            }
        };
        let in_memory_display_name = self.agents.get(agent_id).and_then(AgentTree::display_name);
        let checkpoint_path = self.agent_dir(agent_id.as_str()).join("meta.json");
        let display_projection_bytes = if let Some(display_name) = &in_memory_display_name {
            display_name.len() as u64
        } else {
            fs::metadata(&checkpoint_path)
                .ok()
                .map_or(0, |metadata| metadata.len())
        };
        if record_length.saturating_add(display_projection_bytes) > remaining_bytes {
            return Err(AgentCreationFactsBudgetExceeded);
        }
        let display_name = match in_memory_display_name {
            Some(display_name) => Some(display_name.to_owned()),
            None => read_journal_bound_checkpoint(
                &self.agent_dir(agent_id.as_str()).join("meta.json"),
                agent_id,
                &mut file,
            )
            .ok()
            .and_then(|checkpoint| checkpoint.summary.display_name),
        };
        Ok(agent_creation_facts_from_record(
            agent_id,
            &record,
            display_name,
            record_length.saturating_add(display_projection_bytes),
        ))
    }

    /// Acquires an exclusive flock on the agent's `lock` file if not
    /// already held.
    fn ensure_locked(&mut self, agent_id: &str) -> Result<(), AgentStoreError> {
        let sid = parse_agent_id_for_store(agent_id)?;
        if self.locks.contains_key(&sid) {
            return Ok(());
        }
        let agent_dir = self.agent_dir(agent_id);
        fs::create_dir_all(&agent_dir).map_err(|source| {
            AgentStoreError::CreateParentDirectory {
                path: agent_dir.clone(),
                source,
            }
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&agent_dir, fs::Permissions::from_mode(0o700)).map_err(
                |source| AgentStoreError::Write {
                    path: agent_dir.clone(),
                    source,
                },
            )?;
        }
        let lock_path = agent_dir.join("lock");
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|source| AgentStoreError::Open {
                path: lock_path.clone(),
                source,
            })?;
        if FileExt::try_lock_exclusive(&file).is_err() {
            // Read the holder's `pid=...` line from the same fd we
            // just tried to lock. flock is released by the kernel on
            // process exit, so reaching this branch implies the
            // holder is alive (modulo a thin race window where the
            // holder has the lock but hasn't yet written its pid; in
            // that case `holder` is empty, which Display handles
            // fine).
            let mut holder = String::new();
            let _ = file.read_to_string(&mut holder);
            return Err(AgentStoreError::Locked {
                path: lock_path,
                holder,
            });
        }
        // Replace lock contents with our PID + start time.
        file.set_len(0).map_err(|source| AgentStoreError::Write {
            path: lock_path.clone(),
            source,
        })?;
        file.seek(SeekFrom::Start(0))
            .map_err(|source| AgentStoreError::Write {
                path: lock_path.clone(),
                source,
            })?;
        let pid = std::process::id();
        let now = unix_now();
        writeln!(&mut file, "pid={pid} start={now}").map_err(|source| AgentStoreError::Write {
            path: lock_path.clone(),
            source,
        })?;
        self.locks.insert(sid, file);
        Ok(())
    }

    /// Appends one validated semantic event to the per-agent event stream and
    /// applies it to the in-memory tree.
    ///
    /// Durable agents write the record to disk; ephemeral agents keep the same
    /// record in memory. In both cases the derived [`AgentTree`] is populated
    /// from the accepted record here, so the replay stream and tree cannot
    /// drift.
    ///
    /// Convenience wrapper around
    /// [`AgentStore::append_agent_event_at`] that uses the
    /// agent tree's current head as the fold parent.
    pub fn append_agent_event(
        &mut self,
        agent_id: &str,
        source: Option<ConnectionId>,
        event: Event,
    ) -> Result<AgentAppendOutcome, AgentStoreError> {
        self.append_agent_event_at(
            agent_id,
            source,
            AgentEventParent::InheritHead,
            event,
            UnixMicros::now(),
        )
    }

    /// Like [`AgentStore::append_agent_event`] but folds the
    /// event onto an explicit fold parent instead of the agent
    /// tree's current write cursor. The harness uses this when
    /// publishing on a conversation's behalf, so cross-conversation
    /// events don't have to bounce a shared `head` cursor through
    /// `UiNavigateTree`.
    pub fn append_agent_event_at(
        &mut self,
        agent_id: &str,
        source: Option<ConnectionId>,
        parent: AgentEventParent,
        event: Event,
        recorded_at: UnixMicros,
    ) -> Result<AgentAppendOutcome, AgentStoreError> {
        let sid = parse_agent_id_for_store(agent_id)?;
        let persistence = self.agent_persistence(agent_id);
        if persistence.is_durable() {
            self.ensure_locked(agent_id)?;
        }
        self.load_agent_if_needed(agent_id)?;
        let agent_dir = self.agent_dir(agent_id);
        if persistence.is_durable() {
            fs::create_dir_all(&agent_dir).map_err(|source| {
                AgentStoreError::CreateParentDirectory {
                    path: agent_dir.clone(),
                    source,
                }
            })?;
        }

        let tree = self
            .agents
            .entry(sid.clone())
            .or_insert_with(|| AgentTree::from_events(sid.clone(), &[]));
        if matches!(event, Event::AgentStarted(_)) && tree.next_event_seq().get() != 0 {
            return Err(AgentStoreError::InvalidEvent {
                source: AgentEventValidationError::new(
                    "AgentStarted is only valid as the first journal record",
                ),
            });
        }
        tree.validate_event_at(parent, &event)
            .map_err(|source| AgentStoreError::InvalidEvent { source })?;
        // Cached: `from_events` populated this from the highest
        // persisted sequence at load time; we keep it advanced below.
        // Avoids re-reading and re-decoding the entire on-disk log
        // on every write.
        let next_seq = tree.next_event_seq();
        let record = PersistedAgentEvent {
            seq: next_seq,
            source,
            event: event.clone(),
            parent,
            recorded_at,
        };
        let committed_position = if persistence.is_durable() {
            Some(append_cbor_record(&agent_dir.join("events.cbor"), &record)?)
        } else {
            self.ephemeral_events
                .entry(sid.clone())
                .or_default()
                .push(record.clone());
            None
        };

        let folded_node_id = tree
            .apply_persisted_record(&record)
            .expect("persisted record passed append-time validation");
        if matches!(event, Event::AgentStarted(_)) {
            self.created_agents.insert(sid.clone());
        }
        // Sidecar metadata is derived from the event stream. Do not let a
        // sidecar write failure make the caller retry this already-persisted
        // durable sequence and create a duplicate record.
        if let Some(position) = committed_position {
            let summary = {
                let summary = self.summaries.entry(sid.clone()).or_default();
                summary.apply(&record);
                summary.clone()
            };
            self.publish_checkpoint(&sid, summary, &position);
        } else {
            touch_ephemeral_meta_for_event(
                self.ephemeral_meta.entry(sid).or_default(),
                &event,
                unix_now(),
            );
        }

        Ok(AgentAppendOutcome {
            seq: next_seq,
            folded_node_id,
        })
    }

    /// Append one message fact before any semantic projection consumes it.
    ///
    /// Unlike [`Self::append_agent_event_at`], this path performs no transcript
    /// validation before append. The exact fact becomes the canonical record
    /// first; only afterward does the deterministic post-commit consumer derive
    /// a transcript node (or skip an unprojectable fact), so projection cannot
    /// veto or replace the record.
    ///
    /// # Errors
    ///
    /// Returns [`AgentStoreError`] when `event` is not a `message.*` fact, the
    /// target agent id is invalid, the fact's target differs from the selected
    /// journal owner, or the selected durable or memory-only stream cannot
    /// append the record.
    pub fn append_agent_message_fact_at(
        &mut self,
        agent_id: &str,
        source: Option<ConnectionId>,
        event: Event,
        recorded_at: UnixMicros,
    ) -> Result<AgentAppendOutcome, AgentStoreError> {
        if event.name().category() != &tau_proto::EventCategory::Message {
            return Err(AgentStoreError::UnsupportedRawEvent {
                event_name: event.name(),
            });
        }
        let aid = parse_agent_id_for_store(agent_id)?;
        let claimed_agent_id = event.message_agent_target().cloned();
        if claimed_agent_id.as_ref().map(MessageAgentTarget::as_str) != Some(aid.as_str()) {
            return Err(AgentStoreError::MessageFactTargetMismatch {
                journal_agent_id: aid,
                claimed_agent_id,
            });
        }
        let persistence = self.agent_persistence(agent_id);
        if persistence.is_durable() {
            self.ensure_locked(agent_id)?;
        }
        self.load_agent_if_needed(agent_id)?;
        let agent_dir = self.agent_dir(agent_id);
        if persistence.is_durable() {
            fs::create_dir_all(&agent_dir).map_err(|source| {
                AgentStoreError::CreateParentDirectory {
                    path: agent_dir.clone(),
                    source,
                }
            })?;
        }
        let tree = self
            .agents
            .entry(aid.clone())
            .or_insert_with(|| AgentTree::from_events(aid.clone(), &[]));
        let seq = tree.next_event_seq();
        let record = PersistedAgentEvent {
            seq,
            source,
            event: event.clone(),
            parent: AgentEventParent::InheritHead,
            recorded_at,
        };
        let committed_position = if persistence.is_durable() {
            Some(append_cbor_record(&agent_dir.join("events.cbor"), &record)?)
        } else {
            self.ephemeral_events
                .entry(aid.clone())
                .or_default()
                .push(record.clone());
            None
        };
        let folded_node_id = tree
            .apply_persisted_record(&record)
            .expect("canonical raw fact matches its journal owner and sequence");
        if let Some(position) = committed_position {
            let summary = {
                let summary = self.summaries.entry(aid.clone()).or_default();
                summary.apply(&record);
                summary.clone()
            };
            self.publish_checkpoint(&aid, summary, &position);
        } else {
            touch_ephemeral_meta_for_event(
                self.ephemeral_meta.entry(aid).or_default(),
                &event,
                recorded_at.get() / 1_000_000,
            );
        }
        Ok(AgentAppendOutcome {
            seq,
            folded_node_id,
        })
    }

    /// Validates one prospective event against the currently retained agent
    /// transcript without appending or folding it.
    ///
    /// The harness uses this to turn typed-media quota failures into an
    /// ordinary terminal tool error before publishing a generic success
    /// projection.
    pub fn validate_agent_event_at(
        &mut self,
        agent_id: &str,
        source: Option<ConnectionId>,
        parent: AgentEventParent,
        event: &Event,
        recorded_at: UnixMicros,
    ) -> Result<(), AgentStoreError> {
        let sid = parse_agent_id_for_store(agent_id)?;
        self.load_agent_if_needed(agent_id)?;
        let tree = self
            .agents
            .entry(sid.clone())
            .or_insert_with(|| AgentTree::from_events(sid, &[]));
        tree.validate_event_at(parent, event)
            .map_err(|source| AgentStoreError::InvalidEvent { source })?;
        let prospective_record = PersistedAgentEvent {
            seq: tree.next_event_seq(),
            source,
            event: event.clone(),
            parent,
            recorded_at,
        };
        let mut encoded = Vec::new();
        ciborium::into_writer(&prospective_record, &mut encoded).map_err(|source| {
            AgentStoreError::Encode {
                path: self.agent_dir(agent_id).join("events.cbor"),
                source,
            }
        })?;
        validate_record_length(
            &self.agent_dir(agent_id).join("events.cbor"),
            encoded.len() as u64,
        )
    }

    /// Loads per-agent protocol events from disk or the memory-only replay
    /// stream for ephemeral agents.
    pub fn agent_events(
        &self,
        agent_id: &str,
    ) -> Result<Vec<PersistedAgentEvent>, AgentStoreError> {
        let parsed_agent_id =
            AgentId::parse(agent_id).map_err(|source| AgentStoreError::InvalidAgentId {
                agent_id: agent_id.to_owned(),
                source,
            })?;
        if self.ephemeral_agents.contains(&parsed_agent_id) {
            let events = self
                .ephemeral_events
                .get(&parsed_agent_id)
                .cloned()
                .unwrap_or_default();
            AgentTree::try_from_events(parsed_agent_id, &events)
                .map_err(|source| AgentStoreError::InvalidEvent { source })?;
            return Ok(events);
        }
        let path = self.agent_dir(parsed_agent_id.as_str()).join("events.cbor");
        let events = load_agent_events(&path)?;
        AgentTree::try_from_events(parsed_agent_id, &events)
            .map_err(|source| AgentStoreError::InvalidEvent { source })?;
        Ok(events)
    }

    /// Returns the per-agent storage root this store is rooted at
    /// (typically `<state_dir>/agents/`).
    #[must_use]
    pub fn agents_dir(&self) -> &Path {
        &self.agents_dir
    }

    /// Returns one agent tree if it exists, loading a persisted log
    /// on demand.
    pub fn load_agent(&mut self, agent_id: &str) -> Result<Option<&AgentTree>, AgentStoreError> {
        self.load_agent_if_needed(agent_id)?;
        let Ok(agent_id) = AgentId::parse(agent_id) else {
            return Ok(None);
        };
        Ok(self.agents.get(&agent_id))
    }

    /// Returns one already-loaded agent tree if it exists.
    #[must_use]
    pub fn agent(&self, agent_id: &str) -> Option<&AgentTree> {
        let Ok(agent_id) = AgentId::parse(agent_id) else {
            return None;
        };
        self.agents.get(&agent_id)
    }

    /// Returns all known agents.
    #[must_use]
    pub fn agents(&self) -> Vec<&AgentTree> {
        self.agents.values().collect()
    }

    /// Reads sidecar metadata for one durable or ephemeral agent, if it exists.
    pub fn agent_meta(&self, agent_id: &str) -> io::Result<Option<AgentMeta>> {
        let parsed_agent_id = AgentId::parse(agent_id)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        if self.ephemeral_agents.contains(&parsed_agent_id) {
            return Ok(self.ephemeral_meta.get(&parsed_agent_id).cloned());
        }
        let path = self.agent_dir(parsed_agent_id.as_str()).join("meta.json");
        match read_checkpoint(&path) {
            Ok(checkpoint) if checkpoint.agent_id == parsed_agent_id => {
                Ok(Some(checkpoint.summary.legacy_view()))
            }
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "agent checkpoint id mismatch",
            )),
            Err(error) if error.kind() == io::ErrorKind::InvalidData => match read_meta(&path) {
                Ok(mut meta) => {
                    meta.latest_user_prompt_preview = None;
                    Ok(Some(meta))
                }
                Err(error) => Err(error),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Initializes process-local metadata for an ephemeral agent.
    ///
    /// Durable agents intentionally ignore this legacy compatibility call:
    /// their checkpoint can only be created from journal facts.
    pub fn record_agent_meta(&mut self, agent_id: &str) -> Result<(), AgentStoreError> {
        let aid = parse_agent_id_for_store(agent_id)?;
        if self.ephemeral_agents.contains(&aid) {
            initialize_ephemeral_meta(
                self.ephemeral_meta.entry(aid).or_default(),
                unix_now(),
                true,
            );
            return Ok(());
        }
        // Durable metadata is now exclusively a projection of journal facts.
        // Creation commits `AgentStarted`; a metadata-only identity is forbidden.
        Ok(())
    }

    /// Appends the content-free fact that a human interacted with an agent.
    pub fn record_agent_user_interaction(&mut self, agent_id: &str) -> Result<(), AgentStoreError> {
        let aid = parse_agent_id_for_store(agent_id)?;
        if self.ephemeral_agents.contains(&aid) {
            let now = unix_now();
            let meta = self.ephemeral_meta.entry(aid).or_default();
            initialize_ephemeral_meta(meta, now, false);
            meta.last_user_interaction_time = now;
            return Ok(());
        }
        self.append_agent_event(
            agent_id,
            None,
            Event::AgentUserInteractionRecorded(tau_proto::AgentUserInteractionRecorded {
                agent_id: aid,
            }),
        )
        .map(|_| ())
    }

    fn publish_checkpoint(
        &mut self,
        agent_id: &AgentId,
        summary: AgentSummary,
        position: &CommittedJournalPosition,
    ) {
        // A sidecar is a proof of semantic identity. Creationless artifacts
        // remain reserved and visible, but must go through strict rebuild
        // validation rather than acquiring a trusted Fresh checkpoint.
        if !self.created_agents.contains(agent_id) {
            return;
        }
        let next_seq = self
            .agents
            .get(agent_id)
            .map_or(PersistedAgentEventSeq::new(0), AgentTree::next_event_seq);
        let checkpoint = AgentCheckpoint::new(agent_id.clone(), summary, next_seq, position);
        let path = self.agent_dir(agent_id.as_str()).join("meta.json");
        if let Err(error) = write_checkpoint_atomic(&path, &checkpoint) {
            self.dirty_checkpoints.insert(agent_id.clone());
            eprintln!(
                "tau: agent checkpoint update failed for {}: {error}",
                agent_id.as_str()
            );
        } else {
            self.dirty_checkpoints.remove(agent_id);
        }
    }
}

/// Lists agent metadata across `agents_dir` without taking any flocks.
///
/// Agents whose `meta.json` is missing are skipped silently (the
/// agent may have just been created and not yet touched). A
/// `meta.json` that *exists* but fails to parse is also skipped, but
/// emits a warning to stderr so a corrupt sidecar does not become
/// invisible to operators. The goal is best-effort discovery for
/// `-r` resumption, not strict listing.
pub fn list_agent_metas(agents_dir: &Path) -> io::Result<Vec<(AgentId, AgentMeta)>> {
    crate::list_agent_entries(agents_dir).map(|entries| {
        entries
            .into_iter()
            .filter_map(|entry| {
                entry
                    .summary
                    .map(|summary| (entry.id, summary.legacy_view()))
            })
            .collect()
    })
}

/// Best-effort check whether an agent's lock is currently held.
pub fn agent_is_locked(agents_dir: &Path, agent_id: &str) -> io::Result<bool> {
    let agent_id = AgentId::parse(agent_id)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let lock_path = agents_dir.join(agent_id.as_str()).join("lock");
    let file = match OpenOptions::new().read(true).write(true).open(&lock_path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    match FileExt::try_lock_exclusive(&file) {
        Ok(()) => {
            let _ = FileExt::unlock(&file);
            Ok(false)
        }
        Err(_) => Ok(true),
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_meta(path: &Path) -> io::Result<AgentMeta> {
    const MAX_LEGACY_META_BYTES: u64 = 64 * 1024;
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_LEGACY_META_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_LEGACY_META_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "legacy agent metadata exceeds maximum size",
        ));
    }
    serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn initialize_ephemeral_meta(meta: &mut AgentMeta, now: u64, initialize_user_interaction: bool) {
    if meta.created_at == 0 {
        meta.created_at = now;
    }
    if meta.last_touched == 0 {
        meta.last_touched = now;
    }
    if initialize_user_interaction && meta.last_user_interaction_time == 0 {
        meta.last_user_interaction_time = now;
    }
}

fn touch_ephemeral_meta_for_event(meta: &mut AgentMeta, event: &Event, now: u64) {
    if meta.created_at == 0 {
        meta.created_at = now;
    }
    meta.last_touched = now;
    if let Some(display_name) = display_name_for_event(event).and_then(normalize_display_name) {
        meta.display_name = Some(display_name);
    }
    if let Some(text) = user_prompt_text(event) {
        meta.latest_user_prompt_preview = Some(preview_text(text, 48));
    }
}

fn normalize_display_name(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn display_name_for_event(event: &Event) -> Option<&str> {
    match event {
        Event::AgentStarted(started) => started.display_name.as_deref(),
        Event::AgentDisplayNameSet(name) => Some(&name.display_name),
        _ => None,
    }
}

fn user_prompt_text(event: &Event) -> Option<&str> {
    match event {
        Event::AgentPromptSubmitted(prompt)
            if prompt.originator.is_user() && !prompt.message_class.is_internal() =>
        {
            Some(&prompt.text)
        }
        Event::AgentPromptSteered(steered) if !steered.message_class.is_internal() => {
            Some(&steered.text)
        }
        _ => None,
    }
}

fn preview_text(text: &str, max: usize) -> String {
    let single_line: String = text
        .chars()
        .map(|c| if c == '\n' { ' ' } else { c })
        .collect();
    if single_line.chars().count() < max + 1 {
        single_line
    } else {
        format!("{}…", single_line.chars().take(max).collect::<String>())
    }
}

fn append_cbor_record<T: Serialize>(
    path: &Path,
    record: &T,
) -> Result<CommittedJournalPosition, AgentStoreError> {
    let newly_created = !path.exists();
    let mut encoded = Vec::new();
    ciborium::into_writer(record, &mut encoded).map_err(|source| AgentStoreError::Encode {
        path: path.to_path_buf(),
        source,
    })?;
    let record_length = encoded.len() as u64;
    validate_record_length(path, record_length)?;

    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)
        .map_err(|source| AgentStoreError::Open {
            path: path.to_path_buf(),
            source,
        })?;
    if newly_created {
        // Make the new journal and directory entries durable before any record
        // commit. Failures here are safe to return and retry because no frame
        // bytes have been written.
        sync_parent_directory(path)?;
        if let Some(agent_dir) = path.parent() {
            sync_parent_directory(agent_dir)?;
        }
    }
    let start = journal_position(&mut file).map_err(|source| AgentStoreError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let mut committed_boundary = start.boundary.clone();
    committed_boundary.extend_from_slice(&record_length.to_le_bytes());
    committed_boundary.extend_from_slice(&encoded);
    if committed_boundary.len() > 64 {
        committed_boundary.drain(..committed_boundary.len() - 64);
    }
    file.write_all(&record_length.to_le_bytes())
        .map_err(|source| AgentStoreError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    file.write_all(&encoded)
        .map_err(|source| AgentStoreError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    // Durability: sync_data() guards against the failure mode where
    // the kernel acknowledged the write (length + payload bytes
    // visible) but a crash before flush leaves a torn record on
    // disk. read_cbor_records would then either error or — pre-bound
    // — try to allocate a garbage length on the next read.
    file.sync_data().map_err(|source| AgentStoreError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(CommittedJournalPosition {
        device: start.device,
        inode: start.inode,
        end_offset: start
            .end_offset
            .saturating_add(8)
            .saturating_add(record_length),
        boundary: committed_boundary,
    })
}

fn sync_parent_directory(path: &Path) -> Result<(), AgentStoreError> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| AgentStoreError::Write {
            path: parent.to_path_buf(),
            source,
        })
}

fn validate_record_length(path: &Path, record_length: u64) -> Result<(), AgentStoreError> {
    if MAX_RECORD_BYTES < record_length {
        Err(AgentStoreError::RecordTooLarge {
            path: path.to_path_buf(),
            record_length,
            maximum: MAX_RECORD_BYTES,
        })
    } else {
        Ok(())
    }
}

fn load_agent_events(path: &Path) -> Result<Vec<PersistedAgentEvent>, AgentStoreError> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut events = Vec::new();
    read_cbor_records(path, |record: PersistedAgentEvent| {
        events.push(record);
    })?;
    for (idx, record) in events.iter().enumerate() {
        let expected = PersistedAgentEventSeq::new(idx as u64);
        if record.seq != expected {
            return Err(AgentStoreError::InvalidSequence {
                path: path.to_path_buf(),
                expected,
                actual: record.seq,
            });
        }
    }
    Ok(events)
}

fn records_begin_with_creation(agent_id: &AgentId, events: &[PersistedAgentEvent]) -> bool {
    matches!(
        events.first().map(|record| &record.event),
        Some(Event::AgentStarted(started)) if &started.agent_id == agent_id
    )
}

fn agent_creation_facts_from_record(
    agent_id: &AgentId,
    record: &PersistedAgentEvent,
    display_name: Option<String>,
    bytes_read: u64,
) -> AgentCreationFacts {
    let valid = record.seq == PersistedAgentEventSeq::new(0)
        && records_begin_with_creation(agent_id, std::slice::from_ref(record))
        && AgentTree::try_from_events(agent_id.clone(), std::slice::from_ref(record)).is_ok();
    let Event::AgentStarted(started) = &record.event else {
        return AgentCreationFacts::Invalid { bytes_read };
    };
    if !valid {
        return AgentCreationFacts::Invalid { bytes_read };
    }
    AgentCreationFacts::Available {
        started_at: (record.recorded_at.get() != 0).then_some(record.recorded_at),
        parent_agent: started.parent_agent.clone(),
        role: started.role.clone(),
        display_name,
        bytes_read,
    }
}

fn encoded_size_with_limit<T: Serialize>(value: &T, limit: u64) -> Option<u64> {
    /// Non-retaining serialized-size counter.
    struct Counter {
        /// Bytes accepted so far.
        written: u64,
        /// Largest accepted total.
        limit: u64,
    }
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            if self.written.saturating_add(length) > self.limit {
                return Err(io::Error::new(
                    io::ErrorKind::FileTooLarge,
                    "encoded value exceeds bound",
                ));
            }
            self.written += length;
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter { written: 0, limit };
    tau_proto::encode_message(&mut counter, value)
        .ok()
        .map(|()| counter.written)
}

fn read_cbor_records<T, F>(path: &Path, mut handle: F) -> Result<(), AgentStoreError>
where
    T: for<'de> Deserialize<'de>,
    F: FnMut(T),
{
    let mut file = File::open(path).map_err(|source| AgentStoreError::Open {
        path: path.to_path_buf(),
        source,
    })?;
    loop {
        let Some(record_length) =
            crate::record_log::read_record_length(&mut file).map_err(|source| {
                AgentStoreError::Read {
                    path: path.to_path_buf(),
                    source,
                }
            })?
        else {
            return Ok(());
        };
        if record_length > MAX_RECORD_BYTES {
            return Err(AgentStoreError::Read {
                path: path.to_path_buf(),
                source: io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "record length {record_length} exceeds maximum {MAX_RECORD_BYTES} \
                         (likely a corrupt or torn write)"
                    ),
                ),
            });
        }
        let mut record_bytes = vec![0_u8; record_length as usize];
        file.read_exact(&mut record_bytes)
            .map_err(|source| AgentStoreError::Read {
                path: path.to_path_buf(),
                source,
            })?;
        let record: T = ciborium::from_reader(record_bytes.as_slice()).map_err(|source| {
            AgentStoreError::Decode {
                path: path.to_path_buf(),
                source,
            }
        })?;
        handle(record);
    }
}
