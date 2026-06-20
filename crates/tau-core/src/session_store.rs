//! Session membership store for loaded-agent facts.
//!
//! Durable sessions are membership containers backed by append-only on-disk
//! logs. Ephemeral sessions keep the same folded membership view in memory
//! only. Agent transcripts live in [`crate::AgentStore`] under the global
//! agents directory in both modes.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use tau_proto::{AgentId, ConnectionId, Event, SessionId, UnixMicros};

use crate::session::SessionMeta;

/// Persistence policy for session membership and sidecar state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SessionPersistenceMode {
    /// Write membership events, metadata, and locks under the sessions root.
    #[default]
    Durable,
    /// Keep membership state in memory only and never create session files.
    Ephemeral,
}

impl SessionPersistenceMode {
    /// Returns true when session membership and sidecars should be written to
    /// disk.
    #[must_use]
    pub const fn is_durable(self) -> bool {
        matches!(self, Self::Durable)
    }

    /// Returns true when session membership should remain process-local only.
    #[must_use]
    pub const fn is_ephemeral(self) -> bool {
        matches!(self, Self::Ephemeral)
    }
}

/// Monotonic sequence number in one persisted session membership log.
///
/// This sequence is relative only to one session's `events.cbor` stream: the
/// first membership record in that file has sequence 0, the second has sequence
/// 1, and so on. It is not comparable to the harness runtime event sequence or
/// to [`crate::PersistedAgentEventSeq`]. The value is persisted as corruption
/// detection metadata; replay semantics are still defined by file order, so
/// load code verifies that stored values match their implied position.
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

    /// Returns the next sequence in the same session membership log.
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
    /// Another process holds the exclusive lock for this object.
    Locked { path: PathBuf, holder: String },
    /// A session directory could not be converted to UTF-8.
    InvalidSessionDir { path: PathBuf },
    /// The event is not a session membership fact for this session.
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
            Self::InvalidEvent { message } => {
                write!(f, "invalid session membership event: {message}")
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
            Self::Locked { .. }
            | Self::InvalidSessionDir { .. }
            | Self::InvalidEvent { .. }
            | Self::InvalidSequence { .. } => None,
        }
    }
}

/// Result of one session membership append.
#[derive(Clone, Debug)]
pub struct AppendOutcome {
    /// Sequence assigned to the appended membership fact within this session
    /// log.
    pub seq: PersistedSessionEventSeq,
    /// Session membership events never fold transcript nodes.
    pub folded_node_id: Option<tau_proto::NodeId>,
}

/// One durable session-owned membership fact.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PersistedSessionEvent {
    /// Sequence within this session's durable `events.cbor` membership stream.
    ///
    /// This is persisted to catch reordered, duplicated, or spliced logs during
    /// load. The implied sequence from file order is still authoritative for
    /// replay; load rejects records where this stored value disagrees with the
    /// record's zero-based position.
    pub seq: PersistedSessionEventSeq,
    /// Connection that published the fact, when known.
    pub source: Option<ConnectionId>,
    /// Membership protocol event (`session.agent_loaded` or
    /// `session.agent_unloaded`).
    pub event: Event,
    /// Wall-clock micros since UNIX epoch when the event was appended.
    #[serde(default)]
    pub recorded_at: UnixMicros,
}

/// Folded membership view for one session.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionMembership {
    session_id: SessionId,
    loaded_agents: HashSet<AgentId>,
    next_event_seq: PersistedSessionEventSeq,
}

impl SessionMembership {
    /// Builds a session membership view from durable membership facts.
    #[must_use]
    pub fn from_events(session_id: SessionId, events: &[PersistedSessionEvent]) -> Self {
        let mut tree = Self {
            session_id,
            loaded_agents: HashSet::new(),
            next_event_seq: PersistedSessionEventSeq::new(0),
        };
        for record in events {
            tree.apply_event(&record.event);
            tree.next_event_seq = record.seq.next();
        }
        tree
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

/// Session membership store for loaded-agent facts.
///
/// Durable stores append membership facts to `<sessions_dir>/<session_id>` and
/// maintain the corresponding metadata/lock sidecars. Ephemeral stores keep the
/// folded membership view in memory only: they never create the sessions root,
/// session directories, event logs, metadata, or locks, and `session_events`
/// reports no durable records.
#[derive(Debug)]
pub struct SessionStore {
    sessions_dir: PathBuf,
    sessions: HashMap<SessionId, SessionMembership>,
    locks: HashMap<SessionId, File>,
    mode: SessionPersistenceMode,
}

impl SessionStore {
    /// Opens the session store and eagerly loads existing membership logs.
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

    /// Opens the session store without loading existing membership logs.
    pub fn open_lazy(sessions_dir: impl Into<PathBuf>) -> Result<Self, SessionStoreError> {
        let sessions_dir = sessions_dir.into();
        fs::create_dir_all(&sessions_dir).map_err(|source| {
            SessionStoreError::CreateParentDirectory {
                path: sessions_dir.clone(),
                source,
            }
        })?;
        Ok(Self {
            sessions_dir,
            sessions: HashMap::new(),
            locks: HashMap::new(),
            mode: SessionPersistenceMode::Durable,
        })
    }

    /// Opens an in-memory session store that never reads or writes session
    /// membership state, metadata, or locks below `sessions_dir`.
    ///
    /// The path is retained only so callers can keep using the same layout
    /// helpers for diagnostics; no directory is created by this constructor.
    pub fn open_ephemeral(sessions_dir: impl Into<PathBuf>) -> Result<Self, SessionStoreError> {
        Ok(Self {
            sessions_dir: sessions_dir.into(),
            sessions: HashMap::new(),
            locks: HashMap::new(),
            mode: SessionPersistenceMode::Ephemeral,
        })
    }

    fn session_dir(&self, session_id: &str) -> PathBuf {
        self.sessions_dir.join(session_id)
    }

    fn load_session_if_needed(&mut self, session_id: &str) -> Result<(), SessionStoreError> {
        if self.mode.is_ephemeral() {
            return Ok(());
        }
        let sid = SessionId::from(session_id);
        if self.sessions.contains_key(&sid) {
            return Ok(());
        }
        let path = self.session_dir(session_id).join("events.cbor");
        if !path.exists() {
            return Ok(());
        }
        let events = load_session_events(&path)?;
        self.sessions
            .insert(sid.clone(), SessionMembership::from_events(sid, &events));
        Ok(())
    }

    fn ensure_locked(&mut self, session_id: &str) -> Result<(), SessionStoreError> {
        if self.mode.is_ephemeral() {
            return Ok(());
        }
        let sid = SessionId::from(session_id);
        if self.locks.contains_key(&sid) {
            return Ok(());
        }
        let session_dir = self.session_dir(session_id);
        fs::create_dir_all(&session_dir).map_err(|source| {
            SessionStoreError::CreateParentDirectory {
                path: session_dir.clone(),
                source,
            }
        })?;
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
            let _ = file.read_to_string(&mut holder);
            return Err(SessionStoreError::Locked {
                path: lock_path,
                holder,
            });
        }
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
        Ok(())
    }

    /// Appends one session membership event.
    pub fn append_session_event(
        &mut self,
        session_id: &str,
        source: Option<ConnectionId>,
        event: Event,
    ) -> Result<AppendOutcome, SessionStoreError> {
        self.append_session_event_at(session_id, source, event, UnixMicros::now())
    }

    /// Like [`Self::append_session_event`] with an explicit timestamp.
    pub fn append_session_event_at(
        &mut self,
        session_id: &str,
        source: Option<ConnectionId>,
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

    /// Like [`Self::append_session_event_at`] but lets callers keep a single
    /// membership fact memory-only even when the store itself is durable.
    pub fn append_session_event_at_with_persistence(
        &mut self,
        session_id: &str,
        source: Option<ConnectionId>,
        event: Event,
        recorded_at: UnixMicros,
        event_persistence: SessionPersistenceMode,
    ) -> Result<AppendOutcome, SessionStoreError> {
        validate_membership_event(session_id, &event)?;
        let write_to_disk = self.mode.is_durable() && event_persistence.is_durable();
        if write_to_disk {
            self.ensure_locked(session_id)?;
        }
        self.load_session_if_needed(session_id)?;
        let session_dir = self.session_dir(session_id);
        if write_to_disk {
            fs::create_dir_all(&session_dir).map_err(|source| {
                SessionStoreError::CreateParentDirectory {
                    path: session_dir.clone(),
                    source,
                }
            })?;
        }
        let sid = SessionId::from(session_id);
        let tree = self
            .sessions
            .entry(sid.clone())
            .or_insert_with(|| SessionMembership::from_events(sid, &[]));
        let seq = tree.next_event_seq();
        let record = PersistedSessionEvent {
            seq,
            source,
            event: event.clone(),
            recorded_at,
        };
        if write_to_disk {
            append_cbor_record(&session_dir.join("events.cbor"), &record)?;
        }
        tree.apply_event(&event);
        tree.advance_next_event_seq();
        // Sidecar metadata is derived from the durable event stream. Do not let
        // a sidecar write failure make the caller retry this already-persisted
        // sequence and create a duplicate record.
        if write_to_disk {
            let _ = touch_meta(&session_dir.join("meta.json"));
        }
        Ok(AppendOutcome {
            seq,
            folded_node_id: None,
        })
    }

    /// Loads durable session membership events.
    pub fn session_events(
        &self,
        session_id: &str,
    ) -> Result<Vec<PersistedSessionEvent>, SessionStoreError> {
        if self.mode.is_ephemeral() {
            return Ok(Vec::new());
        }
        load_session_events(&self.session_dir(session_id).join("events.cbor"))
    }

    /// Returns the storage root for session membership containers.
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
        Ok(self.sessions.get(&SessionId::from(session_id)))
    }

    /// Returns one already-loaded session membership view.
    #[must_use]
    pub fn session(&self, session_id: &str) -> Option<&SessionMembership> {
        self.sessions.get(&SessionId::from(session_id))
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
        self.ensure_locked(session_id)?;
        let path = self.session_dir(session_id).join("meta.json");
        let now = unix_now();
        let mut meta = read_meta(&path).unwrap_or_default();
        if meta.created_at == 0 {
            meta.created_at = now;
        }
        meta.last_touched = now;
        write_meta(&path, &meta)
    }
}

fn validate_membership_event(session_id: &str, event: &Event) -> Result<(), SessionStoreError> {
    match event {
        Event::SessionAgentLoaded(loaded) if loaded.session_id == session_id => Ok(()),
        Event::SessionAgentUnloaded(unloaded) if unloaded.session_id == session_id => Ok(()),
        Event::SessionAgentLoaded(_) | Event::SessionAgentUnloaded(_) => {
            Err(SessionStoreError::InvalidEvent {
                message: "membership event session_id did not match target session".to_owned(),
            })
        }
        _ => Err(SessionStoreError::InvalidEvent {
            message: "session store only persists session.agent_loaded/session.agent_unloaded"
                .to_owned(),
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
        out.push((SessionId::from(name), meta));
    }
    Ok(out)
}

/// Best-effort check whether a session lock is currently held.
pub fn session_is_locked(sessions_dir: &Path, session_id: &str) -> io::Result<bool> {
    let lock_path = sessions_dir.join(session_id).join("lock");
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

fn append_cbor_record<T: Serialize>(path: &Path, record: &T) -> Result<(), SessionStoreError> {
    let mut encoded = Vec::new();
    ciborium::into_writer(record, &mut encoded).map_err(|source| SessionStoreError::Encode {
        path: path.to_path_buf(),
        source,
    })?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|source| SessionStoreError::Open {
            path: path.to_path_buf(),
            source,
        })?;
    file.write_all(&(encoded.len() as u64).to_le_bytes())
        .map_err(|source| SessionStoreError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    file.write_all(&encoded)
        .map_err(|source| SessionStoreError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    file.sync_data().map_err(|source| SessionStoreError::Write {
        path: path.to_path_buf(),
        source,
    })
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

const MAX_RECORD_BYTES: u64 = 64 * 1024 * 1024;

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
        let mut length_bytes = [0_u8; 8];
        match file.read_exact(&mut length_bytes) {
            Ok(()) => {}
            Err(source) if source.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(source) => {
                return Err(SessionStoreError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        }
        let record_length = u64::from_le_bytes(length_bytes);
        if record_length > MAX_RECORD_BYTES {
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
        let record = ciborium::from_reader(record_bytes.as_slice()).map_err(|source| {
            SessionStoreError::Decode {
                path: path.to_path_buf(),
                source,
            }
        })?;
        handle(record);
    }
}
