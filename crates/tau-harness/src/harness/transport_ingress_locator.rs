//! Durable global locator for canonical transport-ingress occurrences.
//!
//! The per-agent incoming envelope remains canonical. A checksummed append-only
//! index makes a dedup miss global without repeated all-history scans. An
//! agents-root process lock serializes the dirty-marker → journal → locator
//! transaction across every harness daemon sharing the store.
//!
//! The canonical agent journals are always authority. A clean checksummed
//! log/head pair is only an acceleration view. Reservation holds the process
//! lock, prospectively checks count/bytes/ordering, writes and parent-syncs
//! dirty, then permits journal publication. Success appends and syncs the log,
//! atomically replaces the head, removes and parent-syncs dirty, and releases
//! the lock. Pre-publication cancellation may remove dirty; any
//! possibly-written journal failure retains dirty and latches unavailable. Cold
//! dirty/head/hash/schema mismatch rebuilds under the same lock; rebuild
//! failure is process-sticky.

mod disk;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};

use disk::*;
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use tau_core::AgentStore;
use tau_proto::{AgentId, Event, MessageEndpoint};

use super::{TransportDedupKey, TransportDedupRecord};

const MAX_LOCATOR_RECORDS: usize = 65_536;
const MAX_LOCATOR_BYTES: u64 = 64 * 1024 * 1024;
const MAX_INDEX_RECORD_BYTES: u64 = 256 * 1024;
const LOCATOR_SCHEMA_VERSION: u32 = 2;
const LOCATOR_LOG: &str = ".transport-ingress-locator-v2.log";
const LOCATOR_HEAD: &str = ".transport-ingress-locator-v2.head.cbor";
const LOCATOR_DIRTY: &str = ".transport-ingress-locator-v2.dirty";
const LOCATOR_LOCK: &str = ".transport-ingress-locator-v2.lock";

/// Failure category exposed to the ingress state machine.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LocatorFailure {
    /// Retained history or derived state cannot be read or validated.
    Unavailable,
    /// Multiple canonical occurrences own one key.
    Ambiguous,
    /// A located canonical envelope no longer exists.
    Pruned,
    /// Count, bytes, or concurrent reservation capacity is exhausted.
    Capacity,
    /// A derived-state durability operation failed.
    Durable,
    /// A source sequence is not newer than global retained route history.
    Ordering,
}

/// One global locator lookup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum LocatorLookup {
    /// No retained canonical occurrence owns the key.
    Missing,
    /// One verified canonical occurrence owns the key.
    Found(Box<TransportDedupRecord>),
}

/// Result of atomically reserving a previously missing key.
pub(super) enum LocatorReservation {
    /// Another harness committed the key after the caller's lookup.
    Found(Box<TransportDedupRecord>),
    /// This locator owns the interprocess transaction until commit or cancel.
    Reserved,
}

/// Harness-owned global canonical ingress locator.
pub(super) struct TransportIngressLocator {
    /// Append-only checksummed locator log.
    log_path: PathBuf,
    /// Atomic locator head and integrity watermark.
    head_path: PathBuf,
    /// Parent-durable marker retained across ambiguous publication.
    dirty_path: PathBuf,
    /// Agents-root interprocess transaction lock.
    lock_path: PathBuf,
    /// Loaded exact canonical mappings.
    entries: HashMap<TransportDedupKey, TransportDedupRecord>,
    /// Loaded keys with multiple canonical owners.
    ambiguous: HashSet<TransportDedupKey>,
    /// Agent journals fully verified against loaded mappings.
    verified_agents: HashSet<AgentId>,
    /// Loaded log integrity/capacity watermark.
    head: Option<LocatorHead>,
    /// Whether one complete derived view has loaded.
    initialized: bool,
    /// Process-sticky failure preventing repeated rebuild work.
    failure: Option<LocatorFailure>,
    /// Lock held from reservation through publication completion.
    reservation_lock: Option<File>,
    /// Prospectively encoded locator record for the transaction.
    reserved_record: Option<Vec<u8>>,
    #[cfg(test)]
    rebuild_count: usize,
}

impl TransportIngressLocator {
    /// Creates an unloaded locator beside the global durable agent journals.
    pub(super) fn new(agents_dir: &Path) -> Self {
        Self {
            log_path: agents_dir.join(LOCATOR_LOG),
            head_path: agents_dir.join(LOCATOR_HEAD),
            dirty_path: agents_dir.join(LOCATOR_DIRTY),
            lock_path: agents_dir.join(LOCATOR_LOCK),
            entries: HashMap::new(),
            ambiguous: HashSet::new(),
            verified_agents: HashSet::new(),
            head: None,
            initialized: false,
            failure: None,
            reservation_lock: None,
            reserved_record: None,
            #[cfg(test)]
            rebuild_count: 0,
        }
    }

    /// Performs a globally serialized lookup and verifies the owning agent
    /// once.
    pub(super) fn lookup(
        &mut self,
        store: &AgentStore,
        key: &TransportDedupKey,
    ) -> Result<LocatorLookup, LocatorFailure> {
        self.check_failure()?;
        if self.reservation_lock.is_some() {
            return Err(LocatorFailure::Capacity);
        }
        let lock = self.try_transaction_lock()?;
        self.refresh_under_lock(store)?;
        let result = self.lookup_loaded(store, key);
        drop(lock);
        if let Err(
            error @ (LocatorFailure::Unavailable
            | LocatorFailure::Ambiguous
            | LocatorFailure::Pruned
            | LocatorFailure::Durable),
        ) = &result
        {
            self.failure = Some(*error);
        }
        result
    }

    /// Reserves one missing key while holding the global process lock through
    /// canonical journal publication and locator completion.
    pub(super) fn reserve(
        &mut self,
        store: &AgentStore,
        key: &TransportDedupKey,
        record: &TransportDedupRecord,
    ) -> Result<LocatorReservation, LocatorFailure> {
        self.check_failure()?;
        if self.reservation_lock.is_some() {
            return Err(LocatorFailure::Capacity);
        }
        let lock = self.try_transaction_lock()?;
        self.refresh_under_lock(store)?;
        if self.ambiguous.contains(key) {
            return Err(LocatorFailure::Ambiguous);
        }
        if let Some(existing) = self.entries.get(key).cloned() {
            return Ok(LocatorReservation::Found(Box::new(existing)));
        }
        if self.entries.len().saturating_add(self.ambiguous.len()) >= MAX_LOCATOR_RECORDS {
            return Err(LocatorFailure::Capacity);
        }
        if let Some(ordering) = record.draft.ordering
            && self
                .max_source_sequence(&key.extension_name, &record.draft)?
                .is_some_and(|last| ordering.source_sequence <= last)
        {
            return Err(LocatorFailure::Ordering);
        }
        let bytes = self.encode_next(DiskEntry::Located {
            key: key.clone(),
            record: Box::new(record.clone()),
        })?;
        let head = self.head.as_ref().ok_or(LocatorFailure::Unavailable)?;
        let prospective = head
            .log_bytes
            .checked_add(8)
            .and_then(|value| value.checked_add(bytes.len() as u64))
            .ok_or(LocatorFailure::Capacity)?;
        if prospective > MAX_LOCATOR_BYTES {
            return Err(LocatorFailure::Capacity);
        }
        write_and_sync(&self.dirty_path, b"dirty").map_err(|_| LocatorFailure::Durable)?;
        sync_parent(&self.dirty_path).map_err(|_| LocatorFailure::Durable)?;
        self.reservation_lock = Some(lock);
        self.reserved_record = Some(bytes);
        Ok(LocatorReservation::Reserved)
    }

    /// Completes a reserved transaction after the canonical envelope committed.
    pub(super) fn commit(
        &mut self,
        key: TransportDedupKey,
        record: TransportDedupRecord,
    ) -> Result<(), LocatorFailure> {
        let bytes = self.reserved_record.take().ok_or(LocatorFailure::Durable)?;
        let result = self.commit_inner(bytes, key, record);
        self.reservation_lock.take();
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }

    fn commit_inner(
        &mut self,
        bytes: Vec<u8>,
        key: TransportDedupKey,
        record: TransportDedupRecord,
    ) -> Result<(), LocatorFailure> {
        let decoded: LogRecord =
            tau_proto::decode_message_from_slice(&bytes).map_err(|_| LocatorFailure::Durable)?;
        if decoded.entry
            != (DiskEntry::Located {
                key: key.clone(),
                record: Box::new(record.clone()),
            })
        {
            return Err(LocatorFailure::Durable);
        }
        append_framed(&self.log_path, &bytes).map_err(|_| LocatorFailure::Durable)?;
        let mut head = self.head.clone().ok_or(LocatorFailure::Durable)?;
        head.count = head.count.saturating_add(1);
        head.log_bytes = head
            .log_bytes
            .saturating_add(8)
            .saturating_add(bytes.len() as u64);
        head.last_hash = decoded.hash;
        write_head_atomic(&self.head_path, &head).map_err(|_| LocatorFailure::Durable)?;
        remove_and_sync_parent(&self.dirty_path).map_err(|_| LocatorFailure::Durable)?;
        self.entries.insert(key, record);
        self.head = Some(head);
        Ok(())
    }

    /// Cancels a reservation before any canonical-journal publication attempt.
    pub(super) fn cancel_reservation(&mut self) {
        self.reserved_record = None;
        if remove_and_sync_parent(&self.dirty_path).is_err() {
            self.failure = Some(LocatorFailure::Durable);
        }
        self.reservation_lock.take();
    }

    /// Retains the durable dirty marker after an append whose outcome is
    /// unknown.
    pub(super) fn fail_ambiguous_publish(&mut self) {
        self.reserved_record = None;
        self.failure = Some(LocatorFailure::Unavailable);
        self.reservation_lock.take();
    }

    fn lookup_loaded(
        &mut self,
        store: &AgentStore,
        key: &TransportDedupKey,
    ) -> Result<LocatorLookup, LocatorFailure> {
        if self.ambiguous.contains(key) {
            return Err(LocatorFailure::Ambiguous);
        }
        let Some(record) = self.entries.get(key).cloned() else {
            return Ok(LocatorLookup::Missing);
        };
        if !self.verified_agents.contains(&record.target_agent_id) {
            self.verify_agent(store, &record.target_agent_id)?;
            self.verified_agents.insert(record.target_agent_id.clone());
        }
        Ok(LocatorLookup::Found(Box::new(record)))
    }

    fn verify_agent(&self, store: &AgentStore, agent_id: &AgentId) -> Result<(), LocatorFailure> {
        let expected = self
            .entries
            .iter()
            .filter(|(_, record)| &record.target_agent_id == agent_id)
            .collect::<HashMap<_, _>>();
        let mut found = HashSet::new();
        let mut scan_failure = None;
        store
            .visit_retained_agent_events(agent_id, |persisted| {
                let Event::AgentMessageIncoming(message) = persisted.event else {
                    return true;
                };
                let Some((key, record)) = entry_from_message(message) else {
                    scan_failure = Some(LocatorFailure::Unavailable);
                    return false;
                };
                if let Some(expected_record) = expected.get(&key)
                    && (**expected_record != record || !found.insert(key))
                {
                    scan_failure = Some(LocatorFailure::Ambiguous);
                    return false;
                }
                true
            })
            .map_err(|_| LocatorFailure::Unavailable)?;
        if let Some(error) = scan_failure {
            return Err(error);
        }
        if expected.keys().any(|key| !found.contains(*key)) {
            return Err(LocatorFailure::Pruned);
        }
        Ok(())
    }

    fn refresh_under_lock(&mut self, store: &AgentStore) -> Result<(), LocatorFailure> {
        if try_exists(&self.dirty_path).map_err(|_| LocatorFailure::Unavailable)? {
            return self.rebuild(store);
        }
        let disk_head = match read_head(&self.head_path) {
            Ok(head) => head,
            Err(_) => return self.rebuild(store),
        };
        if self.initialized && self.head.as_ref() == Some(&disk_head) {
            return Ok(());
        }
        match self.load_log(&disk_head) {
            Ok(()) => Ok(()),
            Err(_) => self.rebuild(store),
        }
    }

    fn load_log(&mut self, expected_head: &LocatorHead) -> Result<(), LocatorFailure> {
        if expected_head.version != LOCATOR_SCHEMA_VERSION
            || expected_head.count as usize > MAX_LOCATOR_RECORDS
            || expected_head.log_bytes > MAX_LOCATOR_BYTES
        {
            return Err(LocatorFailure::Unavailable);
        }
        let LoadedLog {
            entries,
            ambiguous,
            head: actual_head,
        } = read_complete_log(&self.log_path)?;
        if &actual_head != expected_head {
            return Err(LocatorFailure::Unavailable);
        }
        self.entries = entries;
        self.ambiguous = ambiguous;
        self.verified_agents.clear();
        self.head = Some(actual_head);
        self.initialized = true;
        Ok(())
    }

    fn rebuild(&mut self, store: &AgentStore) -> Result<(), LocatorFailure> {
        #[cfg(test)]
        {
            self.rebuild_count += 1;
        }
        let result = self.rebuild_inner(store);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }

    fn rebuild_inner(&mut self, store: &AgentStore) -> Result<(), LocatorFailure> {
        let mut entries = HashMap::new();
        let mut ambiguous = HashSet::new();
        let mut encoded_bytes = 0_u64;
        let mut capacity_exceeded = false;
        for agent_id in store
            .retained_agent_ids()
            .map_err(|_| LocatorFailure::Unavailable)?
        {
            let mut scan_failure = false;
            store
                .visit_retained_agent_events(&agent_id, |persisted| {
                    let Event::AgentMessageIncoming(message) = persisted.event else {
                        return true;
                    };
                    let Some((key, record)) = entry_from_message(message) else {
                        scan_failure = true;
                        return false;
                    };
                    let record_bytes = tau_proto::encode_message_to_vec(&(key.clone(), &record))
                        .map_or(MAX_LOCATOR_BYTES.saturating_add(1), |bytes| {
                            bytes.len() as u64
                        });
                    encoded_bytes = encoded_bytes.saturating_add(record_bytes).saturating_add(8);
                    if entries.len().saturating_add(ambiguous.len()) >= MAX_LOCATOR_RECORDS
                        || encoded_bytes > MAX_LOCATOR_BYTES
                    {
                        capacity_exceeded = true;
                        return false;
                    }
                    if entries.insert(key.clone(), record).is_some() {
                        ambiguous.insert(key);
                    }
                    true
                })
                .map_err(|_| LocatorFailure::Unavailable)?;
            if scan_failure {
                return Err(LocatorFailure::Unavailable);
            }
            if capacity_exceeded {
                return Err(LocatorFailure::Capacity);
            }
        }
        if entries.len().saturating_add(ambiguous.len()) > MAX_LOCATOR_RECORDS {
            return Err(LocatorFailure::Capacity);
        }
        let mut disk_entries = entries
            .iter()
            .filter(|(key, _)| !ambiguous.contains(*key))
            .map(|(key, record)| DiskEntry::Located {
                key: key.clone(),
                record: Box::new(record.clone()),
            })
            .chain(
                ambiguous
                    .iter()
                    .cloned()
                    .map(|key| DiskEntry::Ambiguous { key }),
            )
            .collect::<Vec<_>>();
        disk_entries.sort_by_key(entry_sort_key);
        let (bytes, head) = encode_log(disk_entries)?;
        write_atomic(&self.log_path, &bytes).map_err(|_| LocatorFailure::Durable)?;
        write_head_atomic(&self.head_path, &head).map_err(|_| LocatorFailure::Durable)?;
        remove_and_sync_parent(&self.dirty_path).map_err(|_| LocatorFailure::Durable)?;
        self.entries = entries;
        self.ambiguous = ambiguous;
        self.verified_agents.clear();
        self.head = Some(head);
        self.initialized = true;
        Ok(())
    }

    fn encode_next(&self, entry: DiskEntry) -> Result<Vec<u8>, LocatorFailure> {
        let head = self.head.as_ref().ok_or(LocatorFailure::Unavailable)?;
        encode_record(head.count, head.last_hash, entry)
    }

    fn try_transaction_lock(&self) -> Result<File, LocatorFailure> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&self.lock_path)
            .map_err(|_| LocatorFailure::Unavailable)?;
        file.try_lock_exclusive()
            .map_err(|_| LocatorFailure::Capacity)?;
        Ok(file)
    }

    fn check_failure(&self) -> Result<(), LocatorFailure> {
        self.failure.map_or(Ok(()), Err)
    }

    /// Returns the retained global maximum for one extension transport route.
    pub(super) fn max_source_sequence(
        &self,
        extension_name: &tau_proto::ExtensionName,
        draft: &tau_proto::TransportMessageDraft,
    ) -> Result<Option<u64>, LocatorFailure> {
        self.check_failure()?;
        let conversation_id = draft
            .conversation
            .as_ref()
            .and_then(|conversation| conversation.stable_id.as_ref());
        let thread_id = draft
            .conversation
            .as_ref()
            .and_then(|conversation| conversation.thread.as_ref())
            .map(|thread| &thread.stable_id);
        Ok(self
            .entries
            .iter()
            .filter(|(key, record)| {
                &key.extension_name == extension_name
                    && key.transport_name == draft.transport_name
                    && record
                        .draft
                        .conversation
                        .as_ref()
                        .and_then(|conversation| conversation.stable_id.as_ref())
                        == conversation_id
                    && record
                        .draft
                        .conversation
                        .as_ref()
                        .and_then(|conversation| conversation.thread.as_ref())
                        .map(|thread| &thread.stable_id)
                        == thread_id
            })
            .filter_map(|(_, record)| {
                record
                    .draft
                    .ordering
                    .map(|ordering| ordering.source_sequence)
            })
            .max())
    }

    #[cfg(test)]
    fn rebuild_count(&self) -> usize {
        self.rebuild_count
    }
}

fn entry_from_message(
    message: tau_proto::AgentMessageIncoming,
) -> Option<(TransportDedupKey, TransportDedupRecord)> {
    let extension_name = message.envelope.transport.instance.clone()?;
    let dedup_key = message
        .envelope
        .external_identity
        .as_ref()?
        .dedup_key
        .clone()?;
    let (session_id, target_agent_id) = match &message.envelope.destination {
        MessageEndpoint::Agent {
            session_id: Some(session_id),
            agent_id,
            display_name: _,
        } if agent_id == &message.recipient_id => (session_id.clone(), agent_id.clone()),
        MessageEndpoint::Agent {
            session_id: _,
            agent_id: _,
            display_name: _,
        }
        | MessageEndpoint::External {
            stable_id: _,
            display_name: _,
            identity_alias: _,
            actor_kind: _,
        }
        | MessageEndpoint::User => return None,
    };
    let key = TransportDedupKey {
        extension_name,
        transport_name: message.envelope.transport.name.clone(),
        dedup_key,
    };
    let record = TransportDedupRecord {
        draft: super::transport_messages::draft_from_envelope(&message.envelope),
        target_agent_id,
        message_id: message.envelope.message_id,
        committed: true,
        session_id,
    };
    Some((key, record))
}
