//! Session event store for loaded-agent and fallback message facts.
//!
//! Durable sessions are append-only event containers. Their folded view tracks
//! membership while retaining unrouteable message facts in the same journal.
//! Ephemeral sessions keep both records and the folded membership view in
//! memory only. Agent transcripts live in [`crate::AgentStore`] under the
//! global agents directory in both modes.

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use tau_proto::{AgentId, Event, SessionId, UnixMicros};

use crate::record_log::{FramedAppendState, MAX_RECORD_BYTES, missing_directories};
use crate::session::{PersistedEventSource, SessionMeta};

/// Persistence policy for session events and sidecar state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SessionPersistenceMode {
    /// Write session events, metadata, and locks under the sessions root.
    #[default]
    Durable,
    /// Keep session events in memory only and never create session files.
    Ephemeral,
}

impl SessionPersistenceMode {
    /// Returns true when session events and sidecars should be written to disk.
    #[must_use]
    pub const fn is_durable(self) -> bool {
        matches!(self, Self::Durable)
    }

    /// Returns true when session events should remain process-local only.
    #[must_use]
    pub const fn is_ephemeral(self) -> bool {
        matches!(self, Self::Ephemeral)
    }
}

/// Monotonic sequence number in one session-event sequence domain.
///
/// Ordinary records use their zero-based position in a session's logical
/// ordinary stream, persisted as `events.cbor` only in durable mode. Restore
/// records independently use their position in the logical restore stream,
/// persisted as `restore-events.cbor` only in durable mode. A durable session's
/// memory-only ephemeral-agent membership overlay uses a third zero-based
/// process-local domain. These domains are not comparable to one another, the
/// harness runtime event sequence, or [`crate::PersistedAgentEventSeq`]. Stored
/// values provide corruption detection; ordering remains defined by position
/// within the corresponding stream.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PersistedSessionEventSeq(u64);

impl PersistedSessionEventSeq {
    /// Creates a sequence value from its raw integer representation.
    #[must_use]
    pub fn new(v: u64) -> Self {
        Self(v)
    }

    /// Returns the raw integer representation.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }

    /// Returns the next value in the same session-event sequence domain.
    #[must_use]
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

impl std::fmt::Display for PersistedSessionEventSeq {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Errors returned by append-only durable stores.
#[derive(Debug)]
pub enum SessionStoreError {
    /// Failed to create a store directory.
    CreateParentDirectory { path: PathBuf, source: io::Error },
    /// Failed to open a store file.
    Open { path: PathBuf, source: io::Error },
    /// Failed to read a store file.
    Read { path: PathBuf, source: io::Error },
    /// Failed to write a store file.
    Write { path: PathBuf, source: io::Error },
    /// Failed to decode a CBOR record.
    Decode {
        path: PathBuf,
        source: tau_proto::DecodeError,
    },
    /// Failed to encode a CBOR record.
    Encode {
        path: PathBuf,
        source: tau_proto::EncodeError,
    },
    /// Encoded record exceeded the loader's matching allocation bound.
    RecordTooLarge {
        /// Journal selected by the append caller.
        path: PathBuf,
        /// Encoded CBOR payload length.
        record_length: u64,
        /// Maximum payload length accepted by journal readers.
        maximum: u64,
    },
    /// Another process holds the exclusive lock for this object.
    Locked { path: PathBuf, holder: String },
    /// A session directory could not be converted to UTF-8.
    InvalidSessionDir { path: PathBuf },
    /// A session id is not safe to use as one store directory name.
    InvalidSessionId { session_id: String, message: String },
    /// The event is not an accepted session membership or fallback fact.
    InvalidEvent { message: String },
    /// A persisted record sequence does not match its position in the log.
    InvalidSequence {
        path: PathBuf,
        expected: PersistedSessionEventSeq,
        actual: PersistedSessionEventSeq,
    },
}

impl fmt::Display for SessionStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateParentDirectory { path, source } => write!(
                f,
                "failed to create parent directory for session store {}: {source}",
                path.display()
            ),
            Self::Open { path, source } => write!(
                f,
                "failed to open session store {}: {source}",
                path.display()
            ),
            Self::Read { path, source } => write!(
                f,
                "failed to read session store {}: {source}",
                path.display()
            ),
            Self::Write { path, source } => write!(
                f,
                "failed to write session store {}: {source}",
                path.display()
            ),
            Self::Decode { path, source } => write!(
                f,
                "failed to decode session store record from {}: {source}",
                path.display()
            ),
            Self::Encode { path, source } => write!(
                f,
                "failed to encode session store record for {}: {source}",
                path.display()
            ),
            Self::RecordTooLarge {
                path,
                record_length,
                maximum,
            } => write!(
                f,
                "session store record for {} is {record_length} bytes; maximum is {maximum}",
                path.display()
            ),
            Self::Locked { path, holder } => write!(
                f,
                "session lock at {} held by another process ({})",
                path.display(),
                holder.trim()
            ),
            Self::InvalidSessionDir { path } => write!(
                f,
                "invalid session directory name (non-utf8): {}",
                path.display()
            ),
            Self::InvalidSessionId {
                session_id,
                message,
            } => write!(f, "invalid session id `{session_id}`: {message}"),
            Self::InvalidEvent { message } => {
                write!(f, "invalid session event: {message}")
            }
            Self::InvalidSequence {
                path,
                expected,
                actual,
            } => write!(
                f,
                "invalid session event sequence in {}: expected {expected}, got {actual}",
                path.display()
            ),
        }
    }
}

impl Error for SessionStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CreateParentDirectory { source, .. }
            | Self::Open { source, .. }
            | Self::Read { source, .. }
            | Self::Write { source, .. } => Some(source),
            Self::Decode { source, .. } => Some(source),
            Self::Encode { source, .. } => Some(source),
            Self::RecordTooLarge { .. }
            | Self::Locked { .. }
            | Self::InvalidSessionDir { .. }
            | Self::InvalidSessionId { .. }
            | Self::InvalidEvent { .. }
            | Self::InvalidSequence { .. } => None,
        }
    }
}

/// Result of one session event append.
#[derive(Clone, Debug)]
pub struct AppendOutcome {
    /// Sequence assigned within the selected durable or process-local domain.
    pub seq: PersistedSessionEventSeq,
    /// Session events never fold transcript nodes.
    pub folded_node_id: Option<tau_proto::NodeId>,
}

/// One durable or process-local session-owned fact.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PersistedSessionEvent {
    /// Sequence within this record's ordinary, restore, or overlay stream.
    ///
    /// Durable records persist this value to catch reordered, duplicated, or
    /// spliced logs. Ephemeral-agent overlay records use the same positional
    /// check in their independent memory-only stream.
    pub seq: PersistedSessionEventSeq,
    /// Typed publisher provenance, when known.
    pub source: Option<PersistedEventSource>,
    /// Membership/fallback fact for an ordinary stream, or an execution fact
    /// for a restore stream.
    pub event: Event,
    /// Wall-clock micros since UNIX epoch when the event was appended.
    #[serde(default)]
    pub recorded_at: UnixMicros,
}

/// Folded membership view for one session.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionMembership {
    session_id: SessionId,
    loaded_agents: HashSet<AgentId>,
    next_event_seq: PersistedSessionEventSeq,
}

impl SessionMembership {
    /// Builds a session membership view from durable session facts.
    ///
    /// # Panics
    ///
    /// Panics if `events` contains a record that is not a valid membership or
    /// fallback fact for `session_id`. [`SessionStore`] uses the fallible
    /// replay path when loading durable state from disk.
    #[must_use]
    pub fn from_events(session_id: SessionId, events: &[PersistedSessionEvent]) -> Self {
        Self::try_from_events(session_id, events).expect("validated session events")
    }

    fn try_from_events(
        session_id: SessionId,
        events: &[PersistedSessionEvent],
    ) -> Result<Self, SessionStoreError> {
        let mut tree = Self {
            session_id: session_id.clone(),
            loaded_agents: HashSet::new(),
            next_event_seq: PersistedSessionEventSeq::new(0),
        };
        for record in events {
            validate_session_event(session_id.as_str(), &record.event)?;
            tree.apply_event(&record.event);
            tree.next_event_seq = record.seq.next();
        }
        Ok(tree)
    }

    /// Returns the session identifier.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Returns true when `agent_id` is currently loaded in this session.
    #[must_use]
    pub fn contains_agent(&self, agent_id: &AgentId) -> bool {
        self.loaded_agents.contains(agent_id)
    }

    /// Returns currently loaded agents in this session.
    #[must_use]
    pub fn loaded_agents(&self) -> Vec<&AgentId> {
        let mut agents: Vec<_> = self.loaded_agents.iter().collect();
        agents.sort();
        agents
    }

    /// Applies a validated process-local ephemeral membership overlay.
    ///
    /// Overlay records do not advance the durable sequence cursor. They exist
    /// only to reconstruct same-daemon membership after the durable journal has
    /// independently passed validation.
    ///
    /// # Errors
    ///
    /// Returns [`SessionStoreError`] when the overlay contains non-membership
    /// events, durable loaded-agent markers, invalid session targets, sequence
    /// gaps, or an unload without a preceding process-local load.
    pub fn apply_ephemeral_membership_overlay(
        &mut self,
        events: &[PersistedSessionEvent],
    ) -> Result<(), SessionStoreError> {
        validate_ephemeral_membership_overlay(self.session_id.as_str(), events)?;
        for record in events {
            self.apply_event(&record.event);
        }
        Ok(())
    }

    fn next_event_seq(&self) -> PersistedSessionEventSeq {
        self.next_event_seq
    }

    fn advance_next_event_seq(&mut self) {
        self.next_event_seq = self.next_event_seq.next();
    }

    fn apply_event(&mut self, event: &Event) {
        match event {
            Event::SessionAgentLoaded(loaded) if loaded.session_id == self.session_id => {
                self.loaded_agents.insert(loaded.agent_id.clone());
            }
            Event::SessionAgentUnloaded(unloaded) if unloaded.session_id == self.session_id => {
                self.loaded_agents.remove(&unloaded.agent_id);
            }
            _ => {}
        }
    }
}

/// Session event store for loaded-agent and fallback message facts.
///
/// Durable stores append session facts to `<sessions_dir>/<session_id>` and
/// maintain the corresponding metadata/lock sidecars. A durable store also
/// keeps ephemeral-agent membership in a separately sequenced process-local
/// overlay: it composes that overlay only after durable validation for
/// same-daemon replay, and restart discards it. Wholly ephemeral stores keep
/// the folded membership view and session-scoped execution/restore facts in
/// memory only: they never create the sessions root, session directories, event
/// logs, metadata, or locks. Both ordinary session events and execution/restore
/// facts remain available for same-daemon replay.
///
/// Durable frame failures return their original I/O error after an exact-EOF
/// rollback. If truncation cannot restore that EOF, the store poisons only that
/// ordinary or restore journal and rejects later appends before reopening it.
/// Complete frames advance in-memory state immediately while a coalesced worker
/// syncs journals in the background.
///
/// Durable replay and memory-only parity follow
/// `ARCH-tau-core`.
#[derive(Debug)]
pub struct SessionStore {
    sessions_dir: PathBuf,
    /// Failure-atomic append and per-journal poison state.
    framed_appends: FramedAppendState,
    /// Store-root boundary re-covered after the first successful branch lock.
    pending_root_boundary: Option<PathBuf>,
    sessions: HashMap<SessionId, SessionMembership>,
    /// Ordinary replay records retained by a wholly ephemeral session.
    ephemeral_events: HashMap<SessionId, Vec<PersistedSessionEvent>>,
    /// Memory-only membership overlay for ephemeral agents in durable sessions.
    ///
    /// These records use an independent process-local sequence so they cannot
    /// introduce gaps into the durable session journal.
    ephemeral_membership_overlay: HashMap<SessionId, Vec<PersistedSessionEvent>>,
    restore_events: HashMap<SessionId, Vec<PersistedSessionEvent>>,
    locks: HashMap<SessionId, File>,
    /// Sessions whose journal-derived metadata sidecar still needs rebuilding.
    dirty_meta_rebuilds: std::collections::HashSet<SessionId>,
    mode: SessionPersistenceMode,
}

impl SessionStore {
    /// Opens the session store and eagerly loads existing session logs.
    pub fn open(sessions_dir: impl Into<PathBuf>) -> Result<Self, SessionStoreError> {
        let sessions_dir = sessions_dir.into();
        let mut store = Self::open_lazy(sessions_dir.clone())?;
        for entry in fs::read_dir(&sessions_dir).map_err(|source| SessionStoreError::Read {
            path: sessions_dir.clone(),
            source,
        })? {
            let entry = entry.map_err(|source| SessionStoreError::Read {
                path: sessions_dir.clone(),
                source,
            })?;
            let path = entry.path();
            if !path.is_dir() || !path.join("events.cbor").exists() {
                continue;
            }
            let session_id = path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| SessionStoreError::InvalidSessionDir { path: path.clone() })?;
            store.load_session_if_needed(session_id)?;
        }
        Ok(store)
    }

    /// Opens the session store without loading existing session logs.
    pub fn open_lazy(sessions_dir: impl Into<PathBuf>) -> Result<Self, SessionStoreError> {
        let sessions_dir = sessions_dir.into();
        let created_directories = missing_directories(&sessions_dir);
        fs::create_dir_all(&sessions_dir).map_err(|source| {
            SessionStoreError::CreateParentDirectory {
                path: sessions_dir.clone(),
                source,
            }
        })?;
        let mut framed_appends = FramedAppendState::default();
        framed_appends.note_created_directories(created_directories);
        Ok(Self {
            sessions_dir: sessions_dir.clone(),
            framed_appends,
            pending_root_boundary: Some(sessions_dir.clone()),
            sessions: HashMap::new(),
            ephemeral_events: HashMap::new(),
            ephemeral_membership_overlay: HashMap::new(),
            restore_events: HashMap::new(),
            locks: HashMap::new(),
            dirty_meta_rebuilds: std::collections::HashSet::new(),
            mode: SessionPersistenceMode::Durable,
        })
    }

    /// Opens an in-memory session store that never reads or writes session
    /// event state, metadata, or locks below `sessions_dir`.
    ///
    /// The path is retained only so callers can keep using the same layout
    /// helpers for diagnostics; no directory is created by this constructor.
    pub fn open_ephemeral(sessions_dir: impl Into<PathBuf>) -> Result<Self, SessionStoreError> {
        Ok(Self {
            sessions_dir: sessions_dir.into(),
            framed_appends: FramedAppendState::default(),
            pending_root_boundary: None,
            sessions: HashMap::new(),
            ephemeral_events: HashMap::new(),
            ephemeral_membership_overlay: HashMap::new(),
            restore_events: HashMap::new(),
            locks: HashMap::new(),
            dirty_meta_rebuilds: std::collections::HashSet::new(),
            mode: SessionPersistenceMode::Ephemeral,
        })
    }

    fn session_dir(&self, session_id: &SessionId) -> PathBuf {
        self.sessions_dir.join(session_id.as_str())
    }

    fn load_session_if_needed(&mut self, session_id: &str) -> Result<(), SessionStoreError> {
        if self.mode.is_ephemeral() {
            return Ok(());
        }
        let sid = validate_session_id(session_id)?;
        if self.sessions.contains_key(&sid) {
            return Ok(());
        }
        let path = self.session_dir(&sid).join("events.cbor");
        if !path.exists() {
            return Ok(());
        }
        let events = load_session_events(&path)?;
        self.sessions.insert(
            sid.clone(),
            SessionMembership::try_from_events(sid, &events)?,
        );
        Ok(())
    }

    fn ensure_locked(&mut self, session_id: &str) -> Result<bool, SessionStoreError> {
        if self.mode.is_ephemeral() {
            return Ok(false);
        }
        let sid = validate_session_id(session_id)?;
        if self.locks.contains_key(&sid) {
            return Ok(false);
        }
        let session_dir = self.session_dir(&sid);
        let created_directories = missing_directories(&session_dir);
        fs::create_dir_all(&session_dir).map_err(|source| {
            SessionStoreError::CreateParentDirectory {
                path: session_dir.clone(),
                source,
            }
        })?;
        self.framed_appends
            .note_created_directories(created_directories);
        let lock_path = session_dir.join("lock");
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|source| SessionStoreError::Open {
                path: lock_path.clone(),
                source,
            })?;
        if FileExt::try_lock_exclusive(&file).is_err() {
            let mut holder = String::new();
            // This call is intentionally best-effort; preserve the existing discarded
            // result. ast-grep-ignore: let-underscore-call
            let _ = file.read_to_string(&mut holder);
            return Err(SessionStoreError::Locked {
                path: lock_path,
                holder,
            });
        }
        if let Some(root) = self.pending_root_boundary.take() {
            self.framed_appends.note_directory_boundary_chain(&root);
        }
        self.framed_appends.note_directory_boundary(&session_dir);
        file.set_len(0).map_err(|source| SessionStoreError::Write {
            path: lock_path.clone(),
            source,
        })?;
        file.seek(SeekFrom::Start(0))
            .map_err(|source| SessionStoreError::Write {
                path: lock_path.clone(),
                source,
            })?;
        writeln!(&mut file, "pid={} start={}", std::process::id(), unix_now()).map_err(
            |source| SessionStoreError::Write {
                path: lock_path,
                source,
            },
        )?;
        self.locks.insert(sid, file);
        Ok(true)
    }

    /// Appends one session membership or fallback event.
    ///
    /// # Errors
    ///
    /// Returns [`SessionStoreError`] for invalid events or ids, lock and
    /// durable I/O failures, or an ordinary journal poisoned by uncertain
    /// rollback.
    pub fn append_session_event(
        &mut self,
        session_id: &str,
        source: Option<PersistedEventSource>,
        event: Event,
    ) -> Result<AppendOutcome, SessionStoreError> {
        self.append_session_event_at(session_id, source, event, UnixMicros::now())
    }

    /// Like [`Self::append_session_event`] with an explicit timestamp.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::append_session_event`]. A durable
    /// frame failure returns its original error after successful rollback.
    pub fn append_session_event_at(
        &mut self,
        session_id: &str,
        source: Option<PersistedEventSource>,
        event: Event,
        recorded_at: UnixMicros,
    ) -> Result<AppendOutcome, SessionStoreError> {
        self.append_session_event_at_with_persistence(
            session_id,
            source,
            event,
            recorded_at,
            SessionPersistenceMode::Durable,
        )
    }

    /// Like [`Self::append_session_event_at`] with an explicit event policy.
    ///
    /// In a wholly ephemeral store, all accepted session facts remain in
    /// memory. In a durable store, an ephemeral event is restricted to
    /// `SessionAgentLoaded { ephemeral: true, .. }` and a matched unload
    /// lifecycle.
    /// Such records enter a separately sequenced process-local overlay and
    /// never consume a durable journal sequence.
    ///
    /// # Errors
    ///
    /// Returns [`SessionStoreError`] for invalid session facts or identifiers,
    /// non-membership/durable-agent facts requested as ephemeral in a durable
    /// store, unmatched overlay unloads, lock failures, or durable I/O
    /// failures. Rollback uncertainty poisons only the selected durable
    /// journal. Complete frames advance sequences, membership, and metadata
    /// before background writeback.
    pub fn append_session_event_at_with_persistence(
        &mut self,
        session_id: &str,
        source: Option<PersistedEventSource>,
        event: Event,
        recorded_at: UnixMicros,
        event_persistence: SessionPersistenceMode,
    ) -> Result<AppendOutcome, SessionStoreError> {
        let sid = validate_session_id(session_id)?;
        validate_session_event(session_id, &event)?;
        let write_to_disk = self.mode.is_durable() && event_persistence.is_durable();
        let retain_in_memory = self.mode.is_ephemeral();
        let retain_membership_overlay = self.mode.is_durable() && event_persistence.is_ephemeral();
        if retain_membership_overlay {
            validate_ephemeral_membership_overlay_event(session_id, &event)?;
        }
        let session_dir = self.session_dir(&sid);
        let journal_path = session_dir.join("events.cbor");
        if write_to_disk {
            self.framed_appends
                .ensure_appendable(&journal_path)
                .map_err(|source| SessionStoreError::Write {
                    path: journal_path.clone(),
                    source,
                })?;
            let _ = self.lock_and_load_session(session_id)?;
        } else {
            self.load_session_if_needed(session_id)?;
        }
        if write_to_disk {
            fs::create_dir_all(&session_dir).map_err(|source| {
                SessionStoreError::CreateParentDirectory {
                    path: session_dir.clone(),
                    source,
                }
            })?;
        }
        let tree = self
            .sessions
            .entry(sid.clone())
            .or_insert_with(|| SessionMembership::from_events(sid.clone(), &[]));
        let seq = if retain_membership_overlay {
            PersistedSessionEventSeq::new(
                self.ephemeral_membership_overlay
                    .get(&sid)
                    .map_or(0, Vec::len) as u64,
            )
        } else {
            tree.next_event_seq()
        };
        let record = PersistedSessionEvent {
            seq,
            source,
            event: event.clone(),
            recorded_at,
        };
        if retain_membership_overlay {
            let mut candidate = self
                .ephemeral_membership_overlay
                .get(&sid)
                .cloned()
                .unwrap_or_default();
            candidate.push(record.clone());
            validate_ephemeral_membership_overlay(session_id, &candidate)?;
        }
        if write_to_disk {
            append_cbor_record(&mut self.framed_appends, &journal_path, &record)?;
        } else if retain_in_memory {
            self.ephemeral_events
                .entry(sid.clone())
                .or_default()
                .push(record);
        } else if retain_membership_overlay {
            self.ephemeral_membership_overlay
                .entry(sid.clone())
                .or_default()
                .push(record);
        }
        tree.apply_event(&event);
        if write_to_disk || retain_in_memory {
            tree.advance_next_event_seq();
        }
        // Sidecar metadata is derived from the durable event stream. Do not let
        // a sidecar write failure make the caller retry this already-persisted
        // sequence and create a duplicate record.
        if write_to_disk {
            // This call is intentionally best-effort; preserve the existing discarded
            // result. ast-grep-ignore: let-underscore-call
            let _ = touch_meta(&session_dir.join("meta.json"));
        }
        Ok(AppendOutcome {
            seq,
            folded_node_id: None,
        })
    }

    /// Loads durable or same-daemon ephemeral session events.
    pub fn session_events(
        &self,
        session_id: &str,
    ) -> Result<Vec<PersistedSessionEvent>, SessionStoreError> {
        if self.mode.is_ephemeral() {
            let session_id = validate_session_id(session_id)?;
            let events = self
                .ephemeral_events
                .get(&session_id)
                .cloned()
                .unwrap_or_default();
            SessionMembership::try_from_events(session_id, &events)?;
            return Ok(events);
        }
        let session_id = validate_session_id(session_id)?;
        let events = load_session_events(&self.session_dir(&session_id).join("events.cbor"))?;
        SessionMembership::try_from_events(session_id, &events)?;
        Ok(events)
    }

    /// Returns the validated process-local ephemeral-agent membership overlay.
    ///
    /// Durable session replay validates its on-disk snapshot independently,
    /// then applies this strictly membership-only overlay so late
    /// subscribers in the same daemon still see ephemeral agents without
    /// allowing cached durable members to bypass journal validation.
    ///
    /// # Errors
    ///
    /// Returns [`SessionStoreError`] for an invalid session id or an internally
    /// inconsistent overlay sequence, event category, session target, or
    /// load/unload lifecycle.
    pub fn ephemeral_membership_events(
        &self,
        session_id: &str,
    ) -> Result<Vec<PersistedSessionEvent>, SessionStoreError> {
        let session_id = validate_session_id(session_id)?;
        if self.mode.is_ephemeral() {
            return Ok(Vec::new());
        }
        let events = self
            .ephemeral_membership_overlay
            .get(&session_id)
            .cloned()
            .unwrap_or_default();
        validate_ephemeral_membership_overlay(session_id.as_str(), &events)?;
        Ok(events)
    }

    /// Appends one session-scoped execution/restore fact.
    ///
    /// # Errors
    ///
    /// Returns [`SessionStoreError`] for invalid restore facts or session ids,
    /// lock and foreground I/O failures, or a restore journal poisoned by an
    /// unrestored partial write. Locked replay truncates only an incomplete EOF
    /// crash tail before choosing the next sequence; complete invalid frames
    /// fail closed unchanged.
    pub fn append_session_restore_event_at(
        &mut self,
        session_id: &str,
        source: Option<PersistedEventSource>,
        event: Event,
        recorded_at: UnixMicros,
    ) -> Result<(), SessionStoreError> {
        validate_restore_event(&event)?;
        let sid = validate_session_id(session_id)?;
        if self.mode.is_ephemeral() {
            let events = self.restore_events.entry(sid).or_default();
            let seq = PersistedSessionEventSeq::new(events.len() as u64);
            events.push(PersistedSessionEvent {
                seq,
                source,
                event,
                recorded_at,
            });
            return Ok(());
        }
        let path = self.session_dir(&sid).join("restore-events.cbor");
        self.framed_appends
            .ensure_appendable(&path)
            .map_err(|source| SessionStoreError::Write {
                path: path.clone(),
                source,
            })?;
        let _ = self.lock_and_load_session(session_id)?;
        let mut expected_seq = PersistedSessionEventSeq::new(0);
        let recovered = self
            .framed_appends
            .recover(&path, |record: &PersistedSessionEvent| {
                if record.seq != expected_seq || validate_restore_event(&record.event).is_err() {
                    return false;
                }
                expected_seq = expected_seq.next();
                true
            })
            .map_err(|source| SessionStoreError::Read {
                path: path.clone(),
                source,
            })?;
        let events = recovered.records;
        let seq = PersistedSessionEventSeq::new(events.len() as u64);
        append_cbor_record(
            &mut self.framed_appends,
            &path,
            &PersistedSessionEvent {
                seq,
                source,
                event,
                recorded_at,
            },
        )?;
        Ok(())
    }

    /// Loads session-scoped execution/restore facts.
    ///
    /// Durable stores read `<session>/restore-events.cbor`; ephemeral stores
    /// return same-daemon in-memory restore facts.
    pub fn session_restore_events(
        &self,
        session_id: &str,
    ) -> Result<Vec<PersistedSessionEvent>, SessionStoreError> {
        if self.mode.is_ephemeral() {
            let session_id = validate_session_id(session_id)?;
            return Ok(self
                .restore_events
                .get(&session_id)
                .cloned()
                .unwrap_or_default());
        }
        let session_id = validate_session_id(session_id)?;
        let path = self.session_dir(&session_id).join("restore-events.cbor");
        if !path.exists() {
            return Ok(Vec::new());
        }
        let events = load_session_events(&path)?;
        for record in &events {
            validate_restore_event(&record.event)?;
        }
        Ok(events)
    }

    /// Acquires the session writer lock and repairs an incomplete
    /// restore-journal EOF crash tail when present.
    ///
    /// # Errors
    ///
    /// Returns [`SessionStoreError`] for invalid ids, lock or journal I/O
    /// failure, a complete invalid frame, or inability to truncate an
    /// incomplete EOF crash tail.
    pub fn lock_and_recover_session_restore_events(
        &mut self,
        session_id: &str,
    ) -> Result<Vec<PersistedSessionEvent>, SessionStoreError> {
        let sid = validate_session_id(session_id)?;
        let _ = self.lock_and_load_session(session_id)?;
        let path = self.session_dir(&sid).join("restore-events.cbor");
        let mut expected_seq = PersistedSessionEventSeq::new(0);
        self.framed_appends
            .recover(&path, |record: &PersistedSessionEvent| {
                if record.seq != expected_seq || validate_restore_event(&record.event).is_err() {
                    return false;
                }
                expected_seq = expected_seq.next();
                true
            })
            .map(|recovered| recovered.records)
            .map_err(|source| SessionStoreError::Read { path, source })
    }

    /// Returns the storage root for session event containers.
    #[must_use]
    pub fn sessions_dir(&self) -> &Path {
        &self.sessions_dir
    }

    /// Returns one session membership view, loading it on demand.
    pub fn load_session(
        &mut self,
        session_id: &str,
    ) -> Result<Option<&SessionMembership>, SessionStoreError> {
        self.load_session_if_needed(session_id)?;
        let session_id = validate_session_id(session_id)?;
        Ok(self.sessions.get(&session_id))
    }

    /// Acquires the durable session lock before loading its membership view.
    ///
    /// Writers use this ordering when they need the folded view before their
    /// first append. It prevents retention cleanup from deleting the journal
    /// between loading its sequence cursor and acquiring write ownership.
    pub fn lock_and_load_session(
        &mut self,
        session_id: &str,
    ) -> Result<Option<&SessionMembership>, SessionStoreError> {
        let session_id = validate_session_id(session_id)?;
        if self.ensure_locked(session_id.as_str())? {
            self.sessions.remove(&session_id);
        }
        if self.dirty_meta_rebuilds.contains(&session_id) {
            let meta_path = self.session_dir(&session_id).join("meta.json");
            match fs::remove_file(&meta_path) {
                Ok(()) => {
                    self.dirty_meta_rebuilds.remove(&session_id);
                    self.sessions.remove(&session_id);
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    self.dirty_meta_rebuilds.remove(&session_id);
                    self.sessions.remove(&session_id);
                }
                Err(error) => {
                    eprintln!(
                        "tau: failed to retry repaired session metadata rebuild {}: {error}",
                        meta_path.display()
                    );
                }
            }
        }
        if self.mode.is_durable() && !self.sessions.contains_key(&session_id) {
            let path = self.session_dir(&session_id).join("events.cbor");
            let overlay = self.ephemeral_membership_overlay.get(&session_id);
            if path.exists() || overlay.is_some_and(|events| !events.is_empty()) {
                let mut expected_seq = PersistedSessionEventSeq::new(0);
                let recovered = self
                    .framed_appends
                    .recover(&path, |record: &PersistedSessionEvent| {
                        if record.seq != expected_seq
                            || validate_session_event(session_id.as_str(), &record.event).is_err()
                        {
                            return false;
                        }
                        expected_seq = expected_seq.next();
                        true
                    })
                    .map_err(|source| SessionStoreError::Read {
                        path: path.clone(),
                        source,
                    })?;
                let events = recovered.records;
                let meta_path = self.session_dir(&session_id).join("meta.json");
                let rebuilt_meta = SessionMeta {
                    created_at: events
                        .first()
                        .map_or(0, |record| record.recorded_at.get() / 1_000_000),
                    last_touched: events
                        .last()
                        .map_or(0, |record| record.recorded_at.get() / 1_000_000),
                };
                if let Err(error) = write_meta(&meta_path, &rebuilt_meta) {
                    eprintln!(
                        "tau: failed to rebuild session metadata {}: {error}",
                        meta_path.display()
                    );
                    self.dirty_meta_rebuilds.insert(session_id.clone());
                } else {
                    self.dirty_meta_rebuilds.remove(&session_id);
                }
                let mut membership =
                    SessionMembership::try_from_events(session_id.clone(), &events)?;
                if let Some(overlay) = overlay {
                    membership.apply_ephemeral_membership_overlay(overlay)?;
                }
                self.sessions.insert(session_id.clone(), membership);
            }
        } else if self.mode.is_ephemeral() {
            self.load_session_if_needed(session_id.as_str())?;
        }
        Ok(self.sessions.get(&session_id))
    }

    /// Returns one already-loaded session membership view.
    #[must_use]
    pub fn session(&self, session_id: &str) -> Option<&SessionMembership> {
        let Ok(session_id) = validate_session_id(session_id) else {
            return None;
        };
        self.sessions.get(&session_id)
    }

    /// Returns all loaded session membership views.
    #[must_use]
    pub fn sessions(&self) -> Vec<&SessionMembership> {
        self.sessions.values().collect()
    }

    /// Records or refreshes session metadata without storing workspace state.
    pub fn record_session_meta(&mut self, session_id: &str) -> Result<(), SessionStoreError> {
        if self.mode.is_ephemeral() {
            return Ok(());
        }
        let session_id = validate_session_id(session_id)?;
        let _ = self.lock_and_load_session(session_id.as_str())?;
        let path = self.session_dir(&session_id).join("meta.json");
        let now = unix_now();
        let mut meta = read_meta(&path).unwrap_or_default();
        if meta.created_at == 0 {
            meta.created_at = now;
        }
        meta.last_touched = now;
        write_meta(&path, &meta)
    }
}

fn validate_session_id(session_id: &str) -> Result<SessionId, SessionStoreError> {
    SessionId::parse(session_id).map_err(|error| invalid_session_id(session_id, error.to_string()))
}

fn invalid_session_id(session_id: &str, message: impl Into<String>) -> SessionStoreError {
    SessionStoreError::InvalidSessionId {
        session_id: session_id.to_owned(),
        message: message.into(),
    }
}

fn validate_session_event(session_id: &str, event: &Event) -> Result<(), SessionStoreError> {
    match event {
        Event::SessionAgentLoaded(loaded) if loaded.session_id == session_id => Ok(()),
        Event::SessionAgentUnloaded(unloaded) if unloaded.session_id == session_id => Ok(()),
        Event::SessionAgentLoaded(_) | Event::SessionAgentUnloaded(_) => {
            Err(SessionStoreError::InvalidEvent {
                message: "membership event session_id did not match target session".to_owned(),
            })
        }
        _ if event.name().category() == &tau_proto::EventCategory::Message => Ok(()),
        _ => Err(SessionStoreError::InvalidEvent {
            message: "session store only persists membership or fallback message facts".to_owned(),
        }),
    }
}

/// Restrict a durable session's process-local overlay to ephemeral membership.
fn validate_ephemeral_membership_overlay_event(
    session_id: &str,
    event: &Event,
) -> Result<(), SessionStoreError> {
    validate_session_event(session_id, event)?;
    match event {
        Event::SessionAgentLoaded(loaded) if loaded.ephemeral => Ok(()),
        Event::SessionAgentLoaded(_) => Err(SessionStoreError::InvalidEvent {
            message: "process-local membership overlay requires ephemeral loaded agents".to_owned(),
        }),
        Event::SessionAgentUnloaded(_) => Ok(()),
        _ => Err(SessionStoreError::InvalidEvent {
            message: "process-local membership overlay accepts only membership events".to_owned(),
        }),
    }
}

/// Validate one complete process-local overlay, including its local lifecycle.
fn validate_ephemeral_membership_overlay(
    session_id: &str,
    events: &[PersistedSessionEvent],
) -> Result<(), SessionStoreError> {
    let mut loaded = std::collections::HashSet::new();
    for (index, record) in events.iter().enumerate() {
        if record.seq.get() != index as u64 {
            return Err(SessionStoreError::InvalidEvent {
                message: "ephemeral membership overlay has non-sequential records".to_owned(),
            });
        }
        validate_ephemeral_membership_overlay_event(session_id, &record.event)?;
        match &record.event {
            Event::SessionAgentLoaded(event) => {
                loaded.insert(event.agent_id.clone());
            }
            Event::SessionAgentUnloaded(event) if loaded.remove(&event.agent_id) => {}
            Event::SessionAgentUnloaded(_) => {
                return Err(SessionStoreError::InvalidEvent {
                    message:
                        "ephemeral membership overlay unload has no process-local loaded agent"
                            .to_owned(),
                });
            }
            _ => unreachable!("overlay validation accepts only membership events"),
        }
    }
    Ok(())
}

fn validate_restore_event(event: &Event) -> Result<(), SessionStoreError> {
    match event {
        Event::ToolRequest(_) | Event::ToolStarted(_) => Ok(()),
        _ => Err(SessionStoreError::InvalidEvent {
            message: "session restore log only persists tool.request/tool.started".to_owned(),
        }),
    }
}

/// Lists session metadata across `sessions_dir` without taking flocks.
pub fn list_session_metas(sessions_dir: &Path) -> io::Result<Vec<(SessionId, SessionMeta)>> {
    let mut out = Vec::new();
    if !sessions_dir.exists() {
        return Ok(out);
    }
    for entry in fs::read_dir(sessions_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let meta_path = path.join("meta.json");
        let meta = match read_meta(&meta_path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                eprintln!(
                    "tau: skipping session {name}: failed to read {}: {error}",
                    meta_path.display()
                );
                continue;
            }
        };
        let session_id = match validate_session_id(name) {
            Ok(session_id) => session_id,
            Err(error) => {
                eprintln!("tau: skipping session {name}: {error}");
                continue;
            }
        };
        out.push((session_id, meta));
    }
    Ok(out)
}

/// Best-effort check whether a session lock is currently held.
pub fn session_is_locked(sessions_dir: &Path, session_id: &str) -> io::Result<bool> {
    let session_id = validate_session_id(session_id)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let lock_path = sessions_dir.join(session_id.as_str()).join("lock");
    let file = match OpenOptions::new().read(true).write(true).open(&lock_path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    match FileExt::try_lock_exclusive(&file) {
        Ok(()) => {
            // This call is intentionally best-effort; preserve the existing discarded
            // result. ast-grep-ignore: let-underscore-call
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

fn read_meta(path: &Path) -> io::Result<SessionMeta> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn write_meta(path: &Path, meta: &SessionMeta) -> Result<(), SessionStoreError> {
    let bytes = serde_json::to_vec_pretty(meta).map_err(|e| SessionStoreError::Write {
        path: path.to_path_buf(),
        source: io::Error::new(io::ErrorKind::InvalidData, e),
    })?;
    fs::write(path, bytes).map_err(|source| SessionStoreError::Write {
        path: path.to_path_buf(),
        source,
    })
}

fn touch_meta(path: &Path) -> Result<(), SessionStoreError> {
    let now = unix_now();
    let mut meta = read_meta(path).unwrap_or_default();
    if meta.created_at == 0 {
        meta.created_at = now;
    }
    meta.last_touched = now;
    write_meta(path, &meta)
}

fn append_cbor_record<T: Serialize>(
    framed_appends: &mut FramedAppendState,
    path: &Path,
    record: &T,
) -> Result<(), SessionStoreError> {
    framed_appends
        .ensure_appendable(path)
        .map_err(|source| SessionStoreError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    let mut encoded = Vec::new();
    ciborium::into_writer(record, &mut encoded).map_err(|source| SessionStoreError::Encode {
        path: path.to_path_buf(),
        source,
    })?;
    let record_length = encoded.len() as u64;
    validate_record_length(path, record_length)?;
    let newly_created = !path.exists();
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|source| SessionStoreError::Open {
            path: path.to_path_buf(),
            source,
        })?;
    if newly_created {
        framed_appends.note_created_journal(path, &file);
    }
    framed_appends
        .append(path, &mut file, &encoded)
        .map(|_| ())
        .map_err(|source| SessionStoreError::Write {
            path: path.to_path_buf(),
            source,
        })
}

fn validate_record_length(path: &Path, record_length: u64) -> Result<(), SessionStoreError> {
    if MAX_RECORD_BYTES < record_length {
        Err(SessionStoreError::RecordTooLarge {
            path: path.to_path_buf(),
            record_length,
            maximum: MAX_RECORD_BYTES,
        })
    } else {
        Ok(())
    }
}

fn load_session_events(path: &Path) -> Result<Vec<PersistedSessionEvent>, SessionStoreError> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut events = Vec::new();
    read_cbor_records(path, |record: PersistedSessionEvent| events.push(record))?;
    for (idx, record) in events.iter().enumerate() {
        let expected = PersistedSessionEventSeq::new(idx as u64);
        if record.seq != expected {
            return Err(SessionStoreError::InvalidSequence {
                path: path.to_path_buf(),
                expected,
                actual: record.seq,
            });
        }
    }
    Ok(events)
}

fn read_cbor_records<T, F>(path: &Path, mut handle: F) -> Result<(), SessionStoreError>
where
    T: for<'de> Deserialize<'de>,
    F: FnMut(T),
{
    let mut file = File::open(path).map_err(|source| SessionStoreError::Open {
        path: path.to_path_buf(),
        source,
    })?;
    loop {
        let Some(record_length) =
            crate::record_log::read_record_length(&mut file).map_err(|source| {
                SessionStoreError::Read {
                    path: path.to_path_buf(),
                    source,
                }
            })?
        else {
            return Ok(());
        };
        if MAX_RECORD_BYTES < record_length {
            return Err(SessionStoreError::Read {
                path: path.to_path_buf(),
                source: io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "record length {record_length} exceeds maximum {MAX_RECORD_BYTES} (likely a corrupt or torn write)"
                    ),
                ),
            });
        }
        let mut record_bytes = vec![0_u8; record_length as usize];
        file.read_exact(&mut record_bytes)
            .map_err(|source| SessionStoreError::Read {
                path: path.to_path_buf(),
                source,
            })?;
        let mut cursor = io::Cursor::new(record_bytes.as_slice());
        let record =
            ciborium::from_reader(&mut cursor).map_err(|source| SessionStoreError::Decode {
                path: path.to_path_buf(),
                source,
            })?;
        if cursor.position() != record_length {
            return Err(SessionStoreError::Read {
                path: path.to_path_buf(),
                source: io::Error::new(
                    io::ErrorKind::InvalidData,
                    "record payload contains trailing bytes",
                ),
            });
        }
        handle(record);
    }
}

#[cfg(test)]
mod record_bound_tests {
    use tau_proto::{MessageAgentTarget, MessageDelivered, MessageFactId, MessageParty};

    use super::*;

    fn delivered_message(body: String) -> Event {
        Event::MessageDelivered(MessageDelivered::new(
            tau_proto::MessagePublisherId::parse("bridge-main")
                .expect("canonical publisher id must satisfy the identifier grammar"),
            MessageAgentTarget::new("missing-agent"),
            MessageFactId::new("message-1"),
            MessageParty {
                stable_id: "sender-1".to_owned(),
                display_name: None,
                sender_auth: None,
            },
            None,
            body,
        ))
    }

    fn encoded_record_length(body_len: usize, seq: u64) -> usize {
        let record = PersistedSessionEvent {
            seq: PersistedSessionEventSeq::new(seq),
            source: None,
            event: delivered_message("x".repeat(body_len)),
            recorded_at: UnixMicros::new(42),
        };
        let mut encoded = Vec::new();
        ciborium::into_writer(&record, &mut encoded).expect("test record encodes");
        encoded.len()
    }

    fn body_for_encoded_length(encoded_length: usize, seq: u64) -> String {
        const PROBE_BODY_BYTES: usize = 1024 * 1024;
        let overhead = encoded_record_length(PROBE_BODY_BYTES, seq) - PROBE_BODY_BYTES;
        let body = "x".repeat(encoded_length - overhead);
        assert_eq!(
            encoded_record_length(body.len(), seq),
            encoded_length,
            "large-record CBOR overhead should remain stable"
        );
        body
    }

    /// Ensures the session writer accepts the loader's exact maximum rather
    /// than introducing an off-by-one mismatch at the shared boundary.
    #[test]
    fn session_record_limit_accepts_exact_boundary() {
        validate_record_length(Path::new("/not/opened/events.cbor"), MAX_RECORD_BYTES)
            .expect("exact boundary must be accepted");
    }

    /// Ensures a successfully appended session record can be decoded by the
    /// bounded loader after reopening the durable store.
    #[test]
    fn bounded_session_record_round_trips_after_write() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut store = SessionStore::open(temp.path()).expect("store opens");
        let event = delivered_message("read after write".to_owned());
        store
            .append_session_event_at("session-1", None, event.clone(), UnixMicros::new(42))
            .expect("bounded record appends");
        drop(store);

        let reopened = SessionStore::open(temp.path()).expect("written record reloads");
        let events = reopened
            .session_events("session-1")
            .expect("written record reads");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, event);
    }

    /// Ensures an oversized encoded session record is rejected before journal,
    /// folded sequence, or derived metadata state changes.
    #[test]
    fn oversized_session_append_is_atomic() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut store = SessionStore::open(temp.path()).expect("store opens");
        store
            .append_session_event_at(
                "session-1",
                None,
                delivered_message("baseline".to_owned()),
                UnixMicros::new(41),
            )
            .expect("baseline appends");
        let session_dir = temp.path().join("session-1");
        let journal_path = session_dir.join("events.cbor");
        let meta_path = session_dir.join("meta.json");
        write_meta(
            &meta_path,
            &SessionMeta {
                created_at: 1,
                last_touched: 2,
            },
        )
        .expect("write sentinel metadata");
        let journal_before = fs::read(&journal_path).expect("baseline journal");
        let meta_before = fs::read(&meta_path).expect("derived metadata");
        let oversized_body = body_for_encoded_length((MAX_RECORD_BYTES + 1) as usize, 1);

        let error = store
            .append_session_event_at(
                "session-1",
                None,
                delivered_message(oversized_body),
                UnixMicros::new(42),
            )
            .expect_err("oversized record must fail");

        assert!(matches!(
            error,
            SessionStoreError::RecordTooLarge {
                record_length,
                maximum: MAX_RECORD_BYTES,
                ..
            } if record_length == MAX_RECORD_BYTES + 1
        ));
        assert_eq!(
            fs::read(&journal_path).expect("journal remains"),
            journal_before
        );
        assert_eq!(fs::read(&meta_path).expect("metadata remains"), meta_before);
        let outcome = store
            .append_session_event_at(
                "session-1",
                None,
                delivered_message("after rejection".to_owned()),
                UnixMicros::new(43),
            )
            .expect("later bounded record appends");
        assert_eq!(outcome.seq, PersistedSessionEventSeq::new(1));
        assert_eq!(
            store
                .session_events("session-1")
                .expect("journal remains readable")
                .len(),
            2
        );
    }
}
