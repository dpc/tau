//! Stable, read-only snapshots of durable agent journals.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, Read};
#[cfg(unix)]
use std::os::unix::fs as path_std_os_unix_fs;
#[cfg(windows)]
use std::os::windows::fs as path_std_os_windows_fs;
use std::path::{Path, PathBuf};

use fs2::FileExt as Fs2FileExt;
use tau_proto::AgentId;

use super::{AgentStoreError, records_begin_with_creation};
use crate::record_log::{MAX_RECORD_BYTES, read_record_length};
use crate::{AgentTree, PersistedAgentEvent, PersistedAgentEventSeq};

/// An acquired lexical journal lock set that exposes no unvalidated records.
#[derive(Debug)]
pub struct AgentJournalLocks {
    /// Root containing every locked journal.
    agents_dir: std::path::PathBuf,
    /// Locked identities in lexical order.
    agent_ids: BTreeSet<AgentId>,
    /// Shared lock handles retained for the snapshot lifetime.
    _locks: Vec<File>,
}

impl AgentJournalLocks {
    /// Acquires existing journal locks in lexical order without reading
    /// records.
    ///
    /// Acquisition is nonblocking and never creates a directory, lock, or
    /// journal. Duplicate IDs coalesce into one lock; an empty iterator returns
    /// an empty acquired set.
    ///
    /// Callers that discover a multi-agent workflow can recheck discovery under
    /// these retained locks before invoking [`Self::validate`].
    pub fn acquire(
        agents_dir: &Path,
        agent_ids: impl IntoIterator<Item = AgentId>,
    ) -> Result<Self, AgentStoreError> {
        let agent_ids = agent_ids.into_iter().collect::<BTreeSet<_>>();
        let mut locks = Vec::with_capacity(agent_ids.len());
        for agent_id in &agent_ids {
            let lock_path = agents_dir.join(agent_id.as_str()).join("lock");
            let mut lock = open_existing_lock(&lock_path)?;
            if Fs2FileExt::try_lock_shared(&lock).is_err() {
                let mut holder = String::new();
                let _ = lock.by_ref().take(4 * 1024).read_to_string(&mut holder);
                return Err(AgentStoreError::Locked {
                    path: lock_path,
                    holder,
                });
            }
            locks.push(lock);
        }

        Ok(Self {
            agents_dir: agents_dir.to_path_buf(),
            agent_ids,
            _locks: locks,
        })
    }

    /// Strictly validates every complete journal while retaining all locks.
    ///
    /// Validation is all-or-nothing: the method returns no readable snapshot
    /// unless every requested journal passes framing, sequence, creation, and
    /// semantic replay checks.
    ///
    /// Records from one journal are dropped before the next is decoded,
    /// bounding validation memory by the largest included journal rather
    /// than the workflow.
    pub fn validate(self) -> Result<AgentJournalSnapshot, AgentStoreError> {
        let Self {
            agents_dir,
            agent_ids,
            _locks,
        } = self;
        let mut journals = BTreeMap::new();
        for agent_id in &agent_ids {
            let path = agents_dir.join(agent_id.as_str()).join("events.cbor");
            let file = open_existing_journal(&path)?;
            let covered_bytes = journal_len(&file, &path)?;
            journals.insert(
                agent_id.clone(),
                SnapshotJournal {
                    path,
                    file,
                    covered_bytes,
                },
            );
        }
        AgentJournalSnapshot {
            journals,
            _inactive_locks: _locks,
        }
        .validate()
    }
}

/// One opened journal and the exact finite prefix selected for a snapshot.
#[derive(Debug)]
struct SnapshotJournal {
    /// Path used only for typed diagnostics.
    path: PathBuf,
    /// Exact opened journal identity retained for every later read.
    file: File,
    /// Finite byte boundary selected under lock or from a bound checkpoint.
    covered_bytes: u64,
}

/// A fully validated multi-journal snapshot at fixed committed boundaries.
#[derive(Debug)]
pub struct AgentJournalSnapshot {
    /// Open journal identities and their selected finite boundaries.
    journals: BTreeMap<AgentId, SnapshotJournal>,
    /// Locks retained only for journals that were inactive during capture.
    _inactive_locks: Vec<File>,
}

/// Streaming reader for one already-opened, globally validated journal prefix.
pub struct AgentJournalReader<'snapshot> {
    /// Journal path retained for typed diagnostics.
    path: &'snapshot Path,
    /// Exact open journal identity retained by the snapshot.
    file: &'snapshot File,
    /// Current positional-read offset.
    offset: u64,
    /// Bytes remaining before the selected committed boundary.
    remaining_bytes: u64,
    /// Next authoritative sequence expected from the stream.
    expected: PersistedAgentEventSeq,
    /// Whether iteration has reached EOF or an error.
    finished: bool,
}

impl Iterator for AgentJournalReader<'_> {
    type Item = Result<PersistedAgentEvent, AgentStoreError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        if self.remaining_bytes == 0 {
            self.finished = true;
            return None;
        }
        if self.remaining_bytes < 8 {
            return self.fail_read("snapshot boundary ends inside a record length");
        }
        let mut length_bytes = [0; 8];
        if let Err(source) = read_exact_at(self.file, &mut length_bytes, self.offset) {
            self.finished = true;
            return Some(Err(AgentStoreError::Read {
                path: self.path.to_path_buf(),
                source,
            }));
        }
        let length = u64::from_le_bytes(length_bytes);
        if MAX_RECORD_BYTES < length {
            self.finished = true;
            return Some(Err(AgentStoreError::RecordTooLarge {
                path: self.path.to_path_buf(),
                record_length: length,
                maximum: MAX_RECORD_BYTES,
            }));
        }
        if self.remaining_bytes - 8 < length {
            return self.fail_read("snapshot boundary ends inside a record payload");
        }
        let mut bytes = vec![0; length as usize];
        if let Err(source) = read_exact_at(self.file, &mut bytes, self.offset + 8) {
            self.finished = true;
            return Some(Err(AgentStoreError::Read {
                path: self.path.to_path_buf(),
                source,
            }));
        }
        self.offset += 8 + length;
        self.remaining_bytes -= 8 + length;
        let record = match tau_proto::decode_message_from_slice::<PersistedAgentEvent>(&bytes) {
            Ok(record) => record,
            Err(source) => {
                self.finished = true;
                return Some(Err(AgentStoreError::Decode {
                    path: self.path.to_path_buf(),
                    source,
                }));
            }
        };
        if record.seq != self.expected {
            self.finished = true;
            return Some(Err(AgentStoreError::InvalidSequence {
                path: self.path.to_path_buf(),
                expected: self.expected,
                actual: record.seq,
            }));
        }
        self.expected = self.expected.next();
        Some(Ok(record))
    }
}

impl AgentJournalReader<'_> {
    /// Finishes iteration with a typed invalid-boundary read failure.
    fn fail_read(
        &mut self,
        message: &'static str,
    ) -> Option<Result<PersistedAgentEvent, AgentStoreError>> {
        self.finished = true;
        Some(Err(AgentStoreError::Read {
            path: self.path.to_path_buf(),
            source: io::Error::new(io::ErrorKind::UnexpectedEof, message),
        }))
    }
}

impl AgentJournalSnapshot {
    /// Captures and validates finite committed prefixes for requested journals.
    ///
    /// Journals select EOF only after acquiring their shared lock. Active
    /// writers fail closed; managed callers must release, claim
    /// maintenance, capture, and finalize ownership instead of reading
    /// through a checkpoint fallback.
    pub fn capture(
        agents_dir: &Path,
        agent_ids: impl IntoIterator<Item = AgentId>,
    ) -> Result<Self, AgentStoreError> {
        Self::capture_with_before_lock(agents_dir, agent_ids, |_| {})
    }

    /// Testable capture implementation with a hook immediately before lock
    /// acquisition for each journal.
    fn capture_with_before_lock(
        agents_dir: &Path,
        agent_ids: impl IntoIterator<Item = AgentId>,
        mut before_lock: impl FnMut(&AgentId),
    ) -> Result<Self, AgentStoreError> {
        let agent_ids = agent_ids.into_iter().collect::<BTreeSet<_>>();
        let mut journals = BTreeMap::new();
        let mut inactive_locks = Vec::with_capacity(agent_ids.len());
        for agent_id in &agent_ids {
            let agent_dir = agents_dir.join(agent_id.as_str());
            let lock_path = agent_dir.join("lock");
            let lock = open_existing_lock(&lock_path)?;
            before_lock(agent_id);
            let journal_path = agent_dir.join("events.cbor");
            let (journal, retained_lock) = match Fs2FileExt::try_lock_shared(&lock) {
                Ok(()) => {
                    let file = open_existing_journal(&journal_path)?;
                    let covered_bytes = journal_len(&file, &journal_path)?;
                    (
                        SnapshotJournal {
                            path: journal_path,
                            file,
                            covered_bytes,
                        },
                        Some(lock),
                    )
                }
                Err(source) => {
                    return Err(AgentStoreError::Open {
                        path: lock_path,
                        source,
                    });
                }
            };
            journals.insert(agent_id.clone(), journal);
            inactive_locks.extend(retained_lock);
        }
        Self {
            journals,
            _inactive_locks: inactive_locks,
        }
        .validate()
    }

    /// Runs one capture with a deterministic pre-lock race hook.
    #[cfg(test)]
    pub(super) fn capture_for_test(
        agents_dir: &Path,
        agent_ids: impl IntoIterator<Item = AgentId>,
        before_lock: impl FnMut(&AgentId),
    ) -> Result<Self, AgentStoreError> {
        Self::capture_with_before_lock(agents_dir, agent_ids, before_lock)
    }

    /// Streams one included journal after all requested journals validated.
    ///
    /// The returned reader borrows the snapshot's exact opened file identity
    /// and allocates only one length-bounded record at a time.
    pub fn records(&self, agent_id: &AgentId) -> Result<AgentJournalReader<'_>, AgentStoreError> {
        let Some(journal) = self.journals.get(agent_id) else {
            return Err(AgentStoreError::JournalNotIncluded {
                agent_id: agent_id.clone(),
            });
        };
        Ok(AgentJournalReader {
            path: &journal.path,
            file: &journal.file,
            offset: 0,
            remaining_bytes: journal.covered_bytes,
            expected: PersistedAgentEventSeq::new(0),
            finished: false,
        })
    }

    /// Returns included agent identities in lexical order.
    #[must_use]
    pub fn agent_ids(&self) -> impl ExactSizeIterator<Item = &AgentId> {
        self.journals.keys()
    }

    /// Returns whether the snapshot includes the selected agent.
    #[must_use]
    pub fn contains_agent(&self, agent_id: &AgentId) -> bool {
        self.journals.contains_key(agent_id)
    }

    /// Strictly validates every selected prefix before exposing the snapshot.
    fn validate(self) -> Result<Self, AgentStoreError> {
        for agent_id in self.journals.keys() {
            let mut records = self.records(agent_id)?;
            let mut tree = AgentTree::try_from_events(agent_id.clone(), &[])
                .expect("an empty replay initializes a valid tree");
            let mut first = true;
            for record in &mut records {
                let record = record?;
                if first && !records_begin_with_creation(agent_id, std::slice::from_ref(&record)) {
                    return Err(AgentStoreError::InvalidEvent {
                        source: crate::AgentEventValidationError::new(format!(
                            "agent journal `{agent_id}` does not begin with its agent.started fact"
                        )),
                    });
                }
                first = false;
                tree.apply_persisted_record(&record)
                    .map_err(|source| AgentStoreError::InvalidEvent { source })?;
            }
            if first {
                return Err(AgentStoreError::InvalidEvent {
                    source: crate::AgentEventValidationError::new(format!(
                        "agent journal `{agent_id}` is empty"
                    )),
                });
            }
        }
        Ok(self)
    }
}

/// Opens one existing lock file for read-only shared-lock synchronization.
///
/// Agent writers take exclusive locks, so a shared lock excludes them while
/// allowing an immutable snapshot to avoid requesting filesystem write access.
fn open_existing_lock(path: &Path) -> Result<File, AgentStoreError> {
    File::open(path).map_err(|source| missing_or_open(path, source))
}

/// Opens one existing journal file without creating or modifying it.
fn open_existing_journal(path: &Path) -> Result<File, AgentStoreError> {
    File::open(path).map_err(|source| missing_or_open(path, source))
}

/// Preserves missing-journal diagnostics for either required file.
fn missing_or_open(path: &Path, source: io::Error) -> AgentStoreError {
    if source.kind() == io::ErrorKind::NotFound {
        AgentStoreError::JournalMissing {
            path: path.to_path_buf(),
        }
    } else {
        AgentStoreError::Open {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// Returns the current EOF of one already-opened journal.
fn journal_len(file: &File, path: &Path) -> Result<u64, AgentStoreError> {
    file.metadata()
        .map(|metadata| metadata.len())
        .map_err(|source| AgentStoreError::Read {
            path: path.to_path_buf(),
            source,
        })
}

/// Reads one exact byte range without changing the shared file cursor.
fn read_exact_at(file: &File, mut bytes: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !bytes.is_empty() {
        #[cfg(unix)]
        let read = match path_std_os_unix_fs::FileExt::read_at(file, bytes, offset) {
            Err(source) if source.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        #[cfg(windows)]
        let read = match path_std_os_windows_fs::FileExt::seek_read(file, bytes, offset) {
            Err(source) if source.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "journal ended before the selected snapshot boundary",
            ));
        }
        offset += read as u64;
        bytes = &mut bytes[read..];
    }
    Ok(())
}

/// Reads and validates only one agent's bounded sequence-zero creation record.
///
/// This read-only discovery helper never creates paths or reads past the first
/// length-bounded journal record. `Ok(None)` means the journal is missing or
/// empty at the instant of the read. Callers needing a stable multi-agent view
/// must recheck discovery after capturing the selected journal boundaries.
pub fn read_agent_creation_record(
    agents_dir: &Path,
    agent_id: &AgentId,
) -> Result<Option<PersistedAgentEvent>, AgentStoreError> {
    let path = agents_dir.join(agent_id.as_str()).join("events.cbor");
    let mut file = match File::open(&path) {
        Ok(file) => file,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(AgentStoreError::Open { path, source }),
    };
    let Some(length) = read_record_length(&mut file).map_err(|source| AgentStoreError::Read {
        path: path.clone(),
        source,
    })?
    else {
        return Ok(None);
    };
    if MAX_RECORD_BYTES < length {
        return Err(AgentStoreError::RecordTooLarge {
            path,
            record_length: length,
            maximum: MAX_RECORD_BYTES,
        });
    }
    let mut bytes = vec![0; length as usize];
    file.read_exact(&mut bytes)
        .map_err(|source| AgentStoreError::Read {
            path: path.clone(),
            source,
        })?;
    let record =
        tau_proto::decode_message_from_slice::<PersistedAgentEvent>(&bytes).map_err(|source| {
            AgentStoreError::Decode {
                path: path.clone(),
                source,
            }
        })?;
    if record.seq != PersistedAgentEventSeq::new(0) {
        return Err(AgentStoreError::InvalidSequence {
            path,
            expected: PersistedAgentEventSeq::new(0),
            actual: record.seq,
        });
    }
    let valid = records_begin_with_creation(agent_id, std::slice::from_ref(&record))
        && AgentTree::try_from_events(agent_id.clone(), std::slice::from_ref(&record)).is_ok();
    if !valid {
        return Err(AgentStoreError::InvalidEvent {
            source: crate::AgentEventValidationError::new(format!(
                "agent journal `{agent_id}` does not begin with its agent.started fact"
            )),
        });
    }
    Ok(Some(record))
}
