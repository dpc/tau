//! Versioned per-agent summary checkpoints derived from durable journals.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use tau_proto::{AgentId, Event, UnixMicros};

use crate::{AgentMeta, AgentTree, PersistedAgentEvent, PersistedAgentEventSeq};

/// Current on-disk summary checkpoint schema.
pub const AGENT_CHECKPOINT_SCHEMA_VERSION: u32 = 2;
const BOUNDARY_BYTES: u64 = 64;
const MAX_REPAIR_BYTES_PER_AGENT: u64 = 256 * 1024;
const MAX_REPAIR_RECORDS_PER_AGENT: usize = 64;
const MAX_REPAIR_BYTES_PER_LIST: u64 = 1024 * 1024;
const MAX_REPAIR_TIME_PER_LIST: Duration = Duration::from_millis(20);
const MAX_RECORD_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CHECKPOINT_BYTES: u64 = 64 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Summary fields derived only from durable journal records.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentSummary {
    /// Creation timestamp from the first nonzero `AgentStarted` record.
    pub created_at_micros: Option<UnixMicros>,
    /// Timestamp of the latest durable record.
    pub last_touched_at_micros: Option<UnixMicros>,
    /// Timestamp of the latest accepted visible UI interaction fact.
    pub last_user_interaction_at_micros: Option<UnixMicros>,
    /// Current nonblank display name derived from agent lifecycle facts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

impl AgentSummary {
    /// Fold one durable record into this summary projection.
    pub(crate) fn apply(&mut self, record: &PersistedAgentEvent) {
        let timestamp = (record.recorded_at.get() != 0).then_some(record.recorded_at);
        self.last_touched_at_micros = timestamp;
        match &record.event {
            Event::AgentStarted(started) => {
                if self.created_at_micros.is_none() {
                    self.created_at_micros = timestamp;
                }
                if let Some(name) = started.display_name.as_deref().and_then(normalize_name) {
                    self.display_name = Some(name);
                }
            }
            Event::AgentDisplayNameSet(set) => {
                self.display_name = normalize_name(&set.display_name);
            }
            Event::AgentUserInteractionRecorded(_) => {
                self.last_user_interaction_at_micros = timestamp;
            }
            _ => {}
        }
    }

    /// Convert the v2 summary to the legacy public metadata view.
    pub(crate) fn legacy_view(&self) -> AgentMeta {
        AgentMeta {
            created_at: micros_to_seconds(self.created_at_micros),
            last_touched: micros_to_seconds(self.last_touched_at_micros),
            last_user_interaction_time: micros_to_seconds(self.last_user_interaction_at_micros),
            display_name: self.display_name.clone(),
            latest_user_prompt_preview: None,
        }
    }
}

/// Identity and exact prefix covered by one checkpoint.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentJournalCheckpoint {
    /// Device containing the journal file.
    pub device: u64,
    /// Inode of the journal file.
    pub inode: u64,
    /// Exact offset immediately after the last folded frame.
    pub covered_bytes: u64,
    /// Sequence expected for the first suffix record.
    pub next_seq: u64,
    /// Number of bytes used by the boundary witness.
    pub boundary_window_len: u8,
    /// Lowercase BLAKE3-128 digest of the covered prefix boundary.
    pub boundary_blake3_128: String,
}

/// Atomic v2 contents of one per-agent `meta.json`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentCheckpoint {
    /// Checkpoint schema discriminator.
    pub schema_version: u32,
    /// Agent id whose journal produced this checkpoint.
    pub agent_id: AgentId,
    /// Journal prefix identity and watermark.
    pub journal: AgentJournalCheckpoint,
    /// Derived, content-minimized listing summary.
    pub summary: AgentSummary,
}

/// Semantic identity represented by an agent directory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentListIdentity {
    /// A journal exists and supplies semantic identity.
    JournalBacked,
    /// Only a legacy sidecar exists; it reserves the id but is not routable.
    LegacyMetaOnly,
    /// Artifacts exist but cannot currently establish identity.
    UnverifiedArtifact,
}

/// Freshness or recovery state of one listed agent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentListStatus {
    /// Checkpoint identity and exact EOF match.
    Fresh,
    /// A valid checkpoint trails the journal.
    Stale,
    /// An active writer prevented nonblocking repair.
    Busy,
    /// An old unwatermarked sidecar was found.
    Legacy,
    /// No summary file exists.
    MissingSummary,
    /// Summary JSON or schema is invalid.
    CorruptSummary,
    /// Journal identity changed or its length moved behind the watermark.
    ReplacedOrTruncated,
    /// Bounded repair encountered invalid journal data.
    RepairFailed,
}

/// Visible listing result for one valid agent directory name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentListEntry {
    /// Agent directory id.
    pub id: AgentId,
    /// Best available derived or legacy-hint summary.
    pub summary: Option<AgentSummary>,
    /// Strength of semantic identity evidence.
    pub identity: AgentListIdentity,
    /// Checkpoint freshness or recovery state.
    pub status: AgentListStatus,
}

/// Exact committed journal position returned by the append file handle.
#[derive(Clone, Debug)]
pub(crate) struct CommittedJournalPosition {
    /// File device after the durable append.
    pub device: u64,
    /// File inode after the durable append.
    pub inode: u64,
    /// Exact EOF after the appended complete frame.
    pub end_offset: u64,
    /// Boundary bytes ending at `end_offset`.
    pub boundary: Vec<u8>,
}

impl AgentCheckpoint {
    /// Construct a checkpoint after folding the complete current journal.
    pub(crate) fn new(
        agent_id: AgentId,
        summary: AgentSummary,
        next_seq: PersistedAgentEventSeq,
        position: &CommittedJournalPosition,
    ) -> Self {
        Self {
            schema_version: AGENT_CHECKPOINT_SCHEMA_VERSION,
            agent_id,
            journal: AgentJournalCheckpoint {
                device: position.device,
                inode: position.inode,
                covered_bytes: position.end_offset,
                next_seq: next_seq.get(),
                boundary_window_len: u8::try_from(position.boundary.len())
                    .expect("boundary is at most 64 bytes"),
                boundary_blake3_128: boundary_digest(&position.boundary),
            },
            summary,
        }
    }
}

/// Read a v2 checkpoint, rejecting legacy or future schemas.
pub(crate) fn read_checkpoint(path: &Path) -> io::Result<AgentCheckpoint> {
    let bytes = read_bounded_json(path)?;
    let checkpoint: AgentCheckpoint = serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if checkpoint.schema_version != AGENT_CHECKPOINT_SCHEMA_VERSION
        || !checkpoint_is_structurally_valid(&checkpoint)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported agent checkpoint schema",
        ));
    }
    Ok(checkpoint)
}

/// Reads a checkpoint only when its covered journal prefix still matches the
/// exact open journal.
pub(crate) fn read_journal_bound_checkpoint(
    path: &Path,
    agent_id: &AgentId,
    journal: &mut File,
) -> io::Result<AgentCheckpoint> {
    let checkpoint = read_checkpoint(path)?;
    if checkpoint.agent_id != *agent_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "checkpoint agent id does not match journal",
        ));
    }
    let metadata = journal.metadata()?;
    let (device, inode) = metadata_identity(&metadata);
    if device != checkpoint.journal.device
        || inode != checkpoint.journal.inode
        || metadata.len() < checkpoint.journal.covered_bytes
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "checkpoint is not bound to this journal",
        ));
    }
    verify_boundary(journal, &checkpoint.journal)?;
    Ok(checkpoint)
}

/// Atomically replace `meta.json` without exposing partial JSON.
pub(crate) fn write_checkpoint_atomic(path: &Path, checkpoint: &AgentCheckpoint) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "checkpoint has no parent"))?;
    let bytes = serde_json::to_vec_pretty(checkpoint)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let suffix = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp_path = parent.join(format!(".meta.json.{}.{}.tmp", std::process::id(), suffix));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut temp = options.open(&temp_path)?;
        temp.write_all(&bytes)?;
        drop(temp);
        fs::rename(&temp_path, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

/// Return file identity, EOF, and the witness window from one open journal.
pub(crate) fn journal_position(file: &mut File) -> io::Result<CommittedJournalPosition> {
    let metadata = file.metadata()?;
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    #[cfg(unix)]
    let (device, inode) = (metadata.dev(), metadata.ino());
    #[cfg(not(unix))]
    let (device, inode) = (0, 0);
    let end_offset = metadata.len();
    let window_len = end_offset.min(BOUNDARY_BYTES);
    let mut boundary = vec![0; window_len as usize];
    if window_len != 0 {
        file.seek(SeekFrom::Start(end_offset - window_len))?;
        file.read_exact(&mut boundary)?;
        file.seek(SeekFrom::End(0))?;
    }
    Ok(CommittedJournalPosition {
        device,
        inode,
        end_offset,
        boundary,
    })
}

/// Enumerate journal-backed and legacy artifact rows using bounded repair.
pub fn list_agent_entries(agents_dir: &Path) -> io::Result<Vec<AgentListEntry>> {
    list_agent_entries_until(agents_dir, Instant::now() + MAX_REPAIR_TIME_PER_LIST)
}

/// Enumerate agent artifacts with an explicit bounded-repair deadline for
/// tests.
#[cfg(test)]
pub(crate) fn list_agent_entries_until_for_test(
    agents_dir: &Path,
    repair_deadline: Instant,
) -> io::Result<Vec<AgentListEntry>> {
    list_agent_entries_until(agents_dir, repair_deadline)
}

fn list_agent_entries_until(
    agents_dir: &Path,
    repair_deadline: Instant,
) -> io::Result<Vec<AgentListEntry>> {
    let mut entries = Vec::new();
    let mut remaining_repair_bytes = MAX_REPAIR_BYTES_PER_LIST;
    if !agents_dir.exists() {
        return Ok(entries);
    }
    for dir_entry in fs::read_dir(agents_dir)? {
        let dir_entry = dir_entry?;
        if !dir_entry.file_type()?.is_dir() {
            continue;
        }
        let Some(name) = dir_entry.file_name().to_str().map(ToOwned::to_owned) else {
            continue;
        };
        let Ok(id) = AgentId::parse(name) else {
            continue;
        };
        entries.push(inspect_agent_dir(
            id,
            &dir_entry.path(),
            &mut remaining_repair_bytes,
            repair_deadline,
            true,
        ));
    }
    entries.sort_by(|left, right| {
        right
            .summary
            .as_ref()
            .and_then(|summary| summary.last_touched_at_micros)
            .cmp(
                &left
                    .summary
                    .as_ref()
                    .and_then(|summary| summary.last_touched_at_micros),
            )
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(entries)
}

fn inspect_agent_dir(
    id: AgentId,
    dir: &Path,
    remaining_repair_bytes: &mut u64,
    repair_deadline: Instant,
    retry_inconsistent_observation: bool,
) -> AgentListEntry {
    let meta_path = dir.join("meta.json");
    let journal_path = dir.join("events.cbor");
    let checkpoint_result = read_checkpoint(&meta_path);
    let journal_metadata = fs::metadata(&journal_path);
    if journal_metadata.is_err() {
        let legacy = read_legacy_hint(&meta_path);
        return AgentListEntry {
            id,
            summary: legacy,
            identity: if meta_path.exists() {
                AgentListIdentity::LegacyMetaOnly
            } else {
                AgentListIdentity::UnverifiedArtifact
            },
            status: if meta_path.exists() {
                AgentListStatus::Legacy
            } else {
                AgentListStatus::MissingSummary
            },
        };
    }
    let Ok(checkpoint) = checkpoint_result else {
        let legacy_hint = read_legacy_hint(&meta_path);
        let status = if legacy_hint.is_some() {
            AgentListStatus::Legacy
        } else if meta_path.exists() {
            AgentListStatus::CorruptSummary
        } else {
            AgentListStatus::MissingSummary
        };
        return try_bounded_full_rebuild(
            id,
            dir,
            status,
            remaining_repair_bytes,
            repair_deadline,
            legacy_hint,
        );
    };
    if checkpoint.agent_id != id || !checkpoint_is_structurally_valid(&checkpoint) {
        return try_bounded_full_rebuild(
            id,
            dir,
            AgentListStatus::CorruptSummary,
            remaining_repair_bytes,
            repair_deadline,
            None,
        );
    }
    let metadata = journal_metadata.expect("checked above");
    let (device, inode) = metadata_identity(&metadata);
    if device != checkpoint.journal.device
        || inode != checkpoint.journal.inode
        || metadata.len() < checkpoint.journal.covered_bytes
    {
        if retry_inconsistent_observation {
            return inspect_agent_dir(id, dir, remaining_repair_bytes, repair_deadline, false);
        }
        return try_bounded_full_rebuild(
            id,
            dir,
            AgentListStatus::ReplacedOrTruncated,
            remaining_repair_bytes,
            repair_deadline,
            None,
        );
    }
    if metadata.len() == checkpoint.journal.covered_bytes {
        return AgentListEntry {
            id,
            summary: Some(checkpoint.summary),
            identity: AgentListIdentity::JournalBacked,
            status: AgentListStatus::Fresh,
        };
    }
    try_bounded_suffix_repair(id, dir, checkpoint, remaining_repair_bytes, repair_deadline)
}

fn checkpoint_is_structurally_valid(checkpoint: &AgentCheckpoint) -> bool {
    let expected_window = checkpoint.journal.covered_bytes.min(BOUNDARY_BYTES) as u8;
    checkpoint.journal.boundary_window_len == expected_window
        && checkpoint.journal.boundary_blake3_128.len() == 32
        && checkpoint
            .journal
            .boundary_blake3_128
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        && ((checkpoint.journal.covered_bytes == 0 && checkpoint.journal.next_seq == 0)
            || (checkpoint.journal.covered_bytes >= 8 && checkpoint.journal.next_seq != 0))
}

fn try_bounded_suffix_repair(
    id: AgentId,
    dir: &Path,
    checkpoint: AgentCheckpoint,
    remaining_repair_bytes: &mut u64,
    repair_deadline: Instant,
) -> AgentListEntry {
    let fallback = checkpoint.summary.clone();
    let lock = match open_and_try_lock(dir) {
        Ok(lock) => lock,
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            return journal_entry(id, Some(fallback), AgentListStatus::Busy);
        }
        Err(_) => {
            return journal_entry(id, Some(fallback), AgentListStatus::RepairFailed);
        }
    };
    let result = (|| -> io::Result<AgentCheckpoint> {
        let mut checkpoint = read_checkpoint(&dir.join("meta.json"))?;
        if checkpoint.agent_id != id || !checkpoint_is_structurally_valid(&checkpoint) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "checkpoint changed while locking",
            ));
        }
        let mut journal = File::open(dir.join("events.cbor"))?;
        let metadata = journal.metadata()?;
        let (device, inode) = metadata_identity(&metadata);
        if device != checkpoint.journal.device
            || inode != checkpoint.journal.inode
            || metadata.len() < checkpoint.journal.covered_bytes
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "journal identity changed",
            ));
        }
        verify_boundary(&mut journal, &checkpoint.journal)?;
        journal.seek(SeekFrom::Start(checkpoint.journal.covered_bytes))?;
        let suffix_bytes = metadata.len() - checkpoint.journal.covered_bytes;
        if MAX_REPAIR_BYTES_PER_AGENT < suffix_bytes
            || suffix_bytes > *remaining_repair_bytes
            || repair_deadline <= Instant::now()
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "repair byte budget",
            ));
        }
        *remaining_repair_bytes -= suffix_bytes;
        let mut records = 0usize;
        while journal.stream_position()? < metadata.len() {
            if repair_deadline <= Instant::now() {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "repair time budget",
                ));
            }
            if records == MAX_REPAIR_RECORDS_PER_AGENT {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "repair record budget",
                ));
            }
            let remaining_file_bytes = metadata
                .len()
                .saturating_sub(journal.stream_position()?)
                .saturating_sub(8);
            let record = read_one_record(
                &mut journal,
                remaining_file_bytes.min(MAX_REPAIR_BYTES_PER_AGENT),
            )?;
            let expected = PersistedAgentEventSeq::new(checkpoint.journal.next_seq);
            if record.seq != expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid suffix sequence",
                ));
            }
            checkpoint.summary.apply(&record);
            checkpoint.journal.next_seq += 1;
            records += 1;
        }
        let position = journal_position(&mut journal)?;
        checkpoint.journal.covered_bytes = position.end_offset;
        checkpoint.journal.boundary_window_len = position.boundary.len() as u8;
        checkpoint.journal.boundary_blake3_128 = boundary_digest(&position.boundary);
        write_checkpoint_atomic(&dir.join("meta.json"), &checkpoint)?;
        Ok(checkpoint)
    })();
    let _ = FileExt::unlock(&lock);
    match result {
        Ok(checkpoint) => journal_entry(id, Some(checkpoint.summary), AgentListStatus::Fresh),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            journal_entry(id, Some(fallback), AgentListStatus::Stale)
        }
        Err(error) if error.kind() == io::ErrorKind::InvalidData => try_bounded_full_rebuild(
            id,
            dir,
            AgentListStatus::RepairFailed,
            remaining_repair_bytes,
            repair_deadline,
            Some(fallback),
        ),
        Err(_) => journal_entry(id, Some(fallback), AgentListStatus::RepairFailed),
    }
}

fn try_bounded_full_rebuild(
    id: AgentId,
    dir: &Path,
    original_status: AgentListStatus,
    remaining_repair_bytes: &mut u64,
    repair_deadline: Instant,
    fallback_summary: Option<AgentSummary>,
) -> AgentListEntry {
    let lock = match open_and_try_lock(dir) {
        Ok(lock) => lock,
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            return full_rebuild_failure_entry(
                id,
                fallback_summary,
                original_status,
                AgentListStatus::Busy,
            );
        }
        Err(_) => {
            return full_rebuild_failure_entry(
                id,
                fallback_summary,
                original_status,
                AgentListStatus::RepairFailed,
            );
        }
    };
    let result = (|| {
        let metadata = fs::metadata(dir.join("events.cbor"))?;
        if metadata.len() > MAX_REPAIR_BYTES_PER_AGENT
            || metadata.len() > *remaining_repair_bytes
            || repair_deadline <= Instant::now()
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "repair byte budget",
            ));
        }
        *remaining_repair_bytes -= metadata.len();
        rebuild_checkpoint(&id, dir, repair_deadline)
    })();
    let _ = FileExt::unlock(&lock);
    match result {
        Ok(checkpoint) => journal_entry(id, Some(checkpoint.summary), AgentListStatus::Fresh),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            full_rebuild_failure_entry(id, fallback_summary, original_status, original_status)
        }
        Err(error) if error.kind() == io::ErrorKind::InvalidData => AgentListEntry {
            id,
            summary: fallback_summary,
            identity: AgentListIdentity::UnverifiedArtifact,
            status: AgentListStatus::RepairFailed,
        },
        Err(_) => full_rebuild_failure_entry(
            id,
            fallback_summary,
            original_status,
            AgentListStatus::RepairFailed,
        ),
    }
}

/// Strictly rebuild a checkpoint from a complete validated journal.
pub(crate) fn rebuild_checkpoint(
    id: &AgentId,
    dir: &Path,
    repair_deadline: Instant,
) -> io::Result<AgentCheckpoint> {
    let path = dir.join("events.cbor");
    let mut file = File::open(&path)?;
    let stable_len = file.metadata()?.len();
    let mut records = Vec::new();
    loop {
        if repair_deadline <= Instant::now() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "repair time budget",
            ));
        }
        if records.len() == MAX_REPAIR_RECORDS_PER_AGENT {
            if file.stream_position()? < stable_len {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "repair record budget",
                ));
            }
            break;
        }
        let remaining_file_bytes = stable_len
            .saturating_sub(file.stream_position()?)
            .saturating_sub(8);
        let Some(record) = read_one_record_or_eof(
            &mut file,
            remaining_file_bytes.min(MAX_REPAIR_BYTES_PER_AGENT),
        )?
        else {
            break;
        };
        records.push(record);
    }
    for (index, record) in records.iter().enumerate() {
        if record.seq != PersistedAgentEventSeq::new(index as u64) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid sequence",
            ));
        }
    }
    AgentTree::try_from_events(id.clone(), &records)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if !records_begin_with_creation(id, &records) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "agent journal has no matching creation record",
        ));
    }
    let mut summary = AgentSummary::default();
    for record in &records {
        summary.apply(record);
    }
    let position = journal_position(&mut file)?;
    let checkpoint = AgentCheckpoint::new(
        id.clone(),
        summary,
        PersistedAgentEventSeq::new(records.len() as u64),
        &position,
    );
    write_checkpoint_atomic(&dir.join("meta.json"), &checkpoint)?;
    Ok(checkpoint)
}

fn open_and_try_lock(dir: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(dir.join("lock"))?;
    FileExt::try_lock_exclusive(&file)?;
    Ok(file)
}

fn records_begin_with_creation(id: &AgentId, records: &[PersistedAgentEvent]) -> bool {
    matches!(
        records.first().map(|record| &record.event),
        Some(Event::AgentStarted(started)) if &started.agent_id == id
    )
}

fn verify_boundary(file: &mut File, journal: &AgentJournalCheckpoint) -> io::Result<()> {
    let length = u64::from(journal.boundary_window_len);
    if BOUNDARY_BYTES < length || length > journal.covered_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid boundary length",
        ));
    }
    let mut bytes = vec![0; length as usize];
    file.seek(SeekFrom::Start(journal.covered_bytes - length))?;
    file.read_exact(&mut bytes)?;
    if boundary_digest(&bytes) != journal.boundary_blake3_128 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "checkpoint boundary mismatch",
        ));
    }
    Ok(())
}

fn read_one_record(file: &mut File, allocation_budget: u64) -> io::Result<PersistedAgentEvent> {
    read_one_record_or_eof(file, allocation_budget)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "missing record"))
}

fn read_one_record_or_eof(
    file: &mut File,
    allocation_budget: u64,
) -> io::Result<Option<PersistedAgentEvent>> {
    let Some(length) = crate::record_log::read_record_length(file)? else {
        return Ok(None);
    };
    if MAX_RECORD_BYTES < length || allocation_budget < length {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "record exceeds repair budget",
        ));
    }
    let mut bytes = vec![0; length as usize];
    file.read_exact(&mut bytes)?;
    let mut cursor = io::Cursor::new(bytes.as_slice());
    let record = ciborium::from_reader(&mut cursor)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if cursor.position() != length {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "record payload contains trailing bytes",
        ));
    }
    Ok(Some(record))
}

fn read_legacy_hint(path: &Path) -> Option<AgentSummary> {
    let bytes = read_bounded_json(path).ok()?;
    let meta: AgentMeta = serde_json::from_slice(&bytes).ok()?;
    Some(AgentSummary {
        created_at_micros: seconds_to_micros(meta.created_at),
        last_touched_at_micros: seconds_to_micros(meta.last_touched),
        last_user_interaction_at_micros: seconds_to_micros(meta.last_user_interaction_time),
        display_name: meta.display_name,
    })
}

fn read_bounded_json(path: &Path) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_CHECKPOINT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CHECKPOINT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "agent checkpoint exceeds maximum size",
        ));
    }
    Ok(bytes)
}

fn journal_entry(
    id: AgentId,
    summary: Option<AgentSummary>,
    status: AgentListStatus,
) -> AgentListEntry {
    AgentListEntry {
        id,
        summary,
        identity: AgentListIdentity::JournalBacked,
        status,
    }
}

fn unverified_entry(
    id: AgentId,
    summary: Option<AgentSummary>,
    status: AgentListStatus,
) -> AgentListEntry {
    AgentListEntry {
        id,
        summary,
        identity: AgentListIdentity::UnverifiedArtifact,
        status,
    }
}

fn full_rebuild_failure_entry(
    id: AgentId,
    summary: Option<AgentSummary>,
    original_status: AgentListStatus,
    status: AgentListStatus,
) -> AgentListEntry {
    if original_status == AgentListStatus::MissingSummary {
        unverified_entry(id, summary, status)
    } else {
        journal_entry(id, summary, status)
    }
}

fn normalize_name(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn boundary_digest(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex()[..32].to_owned()
}

fn micros_to_seconds(value: Option<UnixMicros>) -> u64 {
    value.map_or(0, |value| value.get() / 1_000_000)
}

fn seconds_to_micros(value: u64) -> Option<UnixMicros> {
    (value != 0).then(|| UnixMicros::new(value.saturating_mul(1_000_000)))
}

fn metadata_identity(metadata: &fs::Metadata) -> (u64, u64) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (metadata.dev(), metadata.ino())
    }
    #[cfg(not(unix))]
    {
        (0, 0)
    }
}
