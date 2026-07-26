//! Stable, read-only snapshots of durable agent journals.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::path::Path;

use fs2::FileExt;
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
    /// Exclusive lock handles retained for the snapshot lifetime.
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
            let mut lock = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&lock_path)
                .map_err(|source| {
                    if source.kind() == io::ErrorKind::NotFound {
                        AgentStoreError::JournalMissing {
                            path: lock_path.clone(),
                        }
                    } else {
                        AgentStoreError::Open {
                            path: lock_path.clone(),
                            source,
                        }
                    }
                })?;
            if FileExt::try_lock_exclusive(&lock).is_err() {
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
        for agent_id in &self.agent_ids {
            let mut records = open_reader(&self, agent_id)?;
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
        Ok(AgentJournalSnapshot { locks: self })
    }
}

/// A fully validated multi-journal snapshot retaining every acquired lock.
#[derive(Debug)]
pub struct AgentJournalSnapshot {
    /// Validated lock-set state.
    locks: AgentJournalLocks,
}

/// Streaming reader for one already-locked and globally validated journal.
pub struct AgentJournalReader<'snapshot> {
    /// Journal path retained for typed diagnostics.
    path: std::path::PathBuf,
    /// Open journal stream.
    file: File,
    /// Next authoritative sequence expected from the stream.
    expected: PersistedAgentEventSeq,
    /// Whether iteration has reached EOF or an error.
    finished: bool,
    /// Prevents the validated snapshot and its locks from being dropped while
    /// this reader remains usable.
    _locks: std::marker::PhantomData<&'snapshot AgentJournalLocks>,
}

impl Iterator for AgentJournalReader<'_> {
    type Item = Result<PersistedAgentEvent, AgentStoreError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        let length = match read_record_length(&mut self.file) {
            Ok(Some(length)) => length,
            Ok(None) => {
                self.finished = true;
                return None;
            }
            Err(source) => {
                self.finished = true;
                return Some(Err(AgentStoreError::Read {
                    path: self.path.clone(),
                    source,
                }));
            }
        };
        if MAX_RECORD_BYTES < length {
            self.finished = true;
            return Some(Err(AgentStoreError::RecordTooLarge {
                path: self.path.clone(),
                record_length: length,
                maximum: MAX_RECORD_BYTES,
            }));
        }
        let mut bytes = vec![0; length as usize];
        if let Err(source) = self.file.read_exact(&mut bytes) {
            self.finished = true;
            return Some(Err(AgentStoreError::Read {
                path: self.path.clone(),
                source,
            }));
        }
        let record = match tau_proto::decode_message_from_slice::<PersistedAgentEvent>(&bytes) {
            Ok(record) => record,
            Err(source) => {
                self.finished = true;
                return Some(Err(AgentStoreError::Decode {
                    path: self.path.clone(),
                    source,
                }));
            }
        };
        if record.seq != self.expected {
            self.finished = true;
            return Some(Err(AgentStoreError::InvalidSequence {
                path: self.path.clone(),
                expected: self.expected,
                actual: record.seq,
            }));
        }
        self.expected = self.expected.next();
        Some(Ok(record))
    }
}

impl AgentJournalSnapshot {
    /// Acquires and validates every requested durable journal.
    pub fn capture(
        agents_dir: &Path,
        agent_ids: impl IntoIterator<Item = AgentId>,
    ) -> Result<Self, AgentStoreError> {
        AgentJournalLocks::acquire(agents_dir, agent_ids)?.validate()
    }

    /// Streams one included journal after all requested journals validated.
    ///
    /// The returned reader borrows this snapshot, so the complete lock set
    /// remains held until the reader is dropped. It allocates only one
    /// length-bounded record at a time.
    pub fn records(&self, agent_id: &AgentId) -> Result<AgentJournalReader<'_>, AgentStoreError> {
        open_reader(&self.locks, agent_id)
    }

    /// Returns locked agent identities in lexical order.
    #[must_use]
    pub fn agent_ids(&self) -> &BTreeSet<AgentId> {
        &self.locks.agent_ids
    }
}

fn checked_agent_path(
    agents_dir: &Path,
    agent_ids: &BTreeSet<AgentId>,
    agent_id: &AgentId,
) -> Result<std::path::PathBuf, AgentStoreError> {
    let path = agents_dir.join(agent_id.as_str()).join("events.cbor");
    if !agent_ids.contains(agent_id) {
        return Err(AgentStoreError::JournalMissing { path });
    }
    if !path.try_exists().map_err(|source| AgentStoreError::Read {
        path: path.clone(),
        source,
    })? {
        return Err(AgentStoreError::JournalMissing { path });
    }
    Ok(path)
}

fn open_reader<'snapshot>(
    locks: &'snapshot AgentJournalLocks,
    agent_id: &AgentId,
) -> Result<AgentJournalReader<'snapshot>, AgentStoreError> {
    let path = checked_agent_path(&locks.agents_dir, &locks.agent_ids, agent_id)?;
    let file = File::open(&path).map_err(|source| AgentStoreError::Open {
        path: path.clone(),
        source,
    })?;
    Ok(AgentJournalReader {
        path,
        file,
        expected: PersistedAgentEventSeq::new(0),
        finished: false,
        _locks: std::marker::PhantomData,
    })
}

/// Reads and validates only one agent's bounded sequence-zero creation record.
///
/// This read-only discovery helper never creates paths or reads past the first
/// length-bounded journal record. `Ok(None)` means the journal is missing or
/// empty at the instant of the read. Callers needing a stable multi-agent view
/// must recheck discovery after acquiring the corresponding journal locks.
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
