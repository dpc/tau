//! Checksummed append-only locator disk format and atomic file operations.

use super::*;

/// One located canonical mapping or a durable ambiguity tombstone.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub(super) enum DiskEntry {
    /// One exact key-to-canonical mapping.
    Located {
        /// Global transport dedup scope.
        key: TransportDedupKey,
        /// Canonical first record.
        record: Box<TransportDedupRecord>,
    },
    /// Multiple retained envelopes own this key.
    Ambiguous {
        /// Conflicted global transport dedup scope.
        key: TransportDedupKey,
    },
}

/// One length-framed hash-chain record.
#[derive(Serialize, Deserialize)]
pub(super) struct LogRecord {
    /// Zero-based append sequence.
    pub(super) sequence: u64,
    /// Hash of the previous record, or zero for the first.
    pub(super) previous_hash: [u8; 32],
    /// Located mapping or ambiguity.
    pub(super) entry: DiskEntry,
    /// Hash of sequence, predecessor, and entry.
    pub(super) hash: [u8; 32],
}

/// Atomic integrity and capacity watermark for the append log.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(super) struct LocatorHead {
    /// Persisted schema version.
    pub(super) version: u32,
    /// Number of complete records.
    pub(super) count: u64,
    /// Exact framed log byte length.
    pub(super) log_bytes: u64,
    /// Last record hash.
    pub(super) last_hash: [u8; 32],
}

/// Fully decoded and hash-verified locator log.
pub(super) struct LoadedLog {
    /// Unique canonical mappings.
    pub(super) entries: HashMap<TransportDedupKey, TransportDedupRecord>,
    /// Ambiguous keys.
    pub(super) ambiguous: HashSet<TransportDedupKey>,
    /// Recomputed head.
    pub(super) head: LocatorHead,
}

/// Encodes one hash-chained locator record within the per-record bound.
pub(super) fn encode_record(
    sequence: u64,
    previous_hash: [u8; 32],
    entry: DiskEntry,
) -> Result<Vec<u8>, LocatorFailure> {
    let hash_input = tau_proto::encode_message_to_vec(&(sequence, previous_hash, &entry))
        .map_err(|_| LocatorFailure::Capacity)?;
    let hash = *blake3::hash(&hash_input).as_bytes();
    let bytes = tau_proto::encode_message_to_vec(&LogRecord {
        sequence,
        previous_hash,
        entry,
        hash,
    })
    .map_err(|_| LocatorFailure::Capacity)?;
    if bytes.len() as u64 > MAX_INDEX_RECORD_BYTES {
        return Err(LocatorFailure::Capacity);
    }
    Ok(bytes)
}

/// Encodes a complete bounded locator log and its integrity head.
pub(super) fn encode_log(
    entries: Vec<DiskEntry>,
) -> Result<(Vec<u8>, LocatorHead), LocatorFailure> {
    let mut output = Vec::new();
    let mut previous_hash = [0; 32];
    for (index, entry) in entries.into_iter().enumerate() {
        let bytes = encode_record(index as u64, previous_hash, entry)?;
        output.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
        output.extend_from_slice(&bytes);
        let record: LogRecord =
            tau_proto::decode_message_from_slice(&bytes).map_err(|_| LocatorFailure::Durable)?;
        previous_hash = record.hash;
        if output.len() as u64 > MAX_LOCATOR_BYTES {
            return Err(LocatorFailure::Capacity);
        }
    }
    let count = count_records(&output)?;
    let log_bytes = output.len() as u64;
    Ok((
        output,
        LocatorHead {
            version: LOCATOR_SCHEMA_VERSION,
            count,
            log_bytes,
            last_hash: previous_hash,
        },
    ))
}

/// Streams and verifies every framed record against its hash chain.
pub(super) fn read_complete_log(path: &Path) -> Result<LoadedLog, LocatorFailure> {
    let mut file = File::open(path).map_err(|_| LocatorFailure::Unavailable)?;
    let size = file
        .metadata()
        .map_err(|_| LocatorFailure::Unavailable)?
        .len();
    if size > MAX_LOCATOR_BYTES {
        return Err(LocatorFailure::Unavailable);
    }
    let mut entries = HashMap::new();
    let mut ambiguous = HashSet::new();
    let mut previous_hash = [0; 32];
    let mut count = 0_u64;
    loop {
        let mut length = [0_u8; 8];
        match file.read_exact(&mut length) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                if file
                    .stream_position()
                    .map_err(|_| LocatorFailure::Unavailable)?
                    == size
                {
                    break;
                }
                return Err(LocatorFailure::Unavailable);
            }
            Err(_) => return Err(LocatorFailure::Unavailable),
        }
        let length = u64::from_be_bytes(length);
        if length > MAX_INDEX_RECORD_BYTES {
            return Err(LocatorFailure::Unavailable);
        }
        let mut bytes = vec![0; length as usize];
        file.read_exact(&mut bytes)
            .map_err(|_| LocatorFailure::Unavailable)?;
        let record: LogRecord = tau_proto::decode_message_from_slice(&bytes)
            .map_err(|_| LocatorFailure::Unavailable)?;
        let expected = encode_record(record.sequence, record.previous_hash, record.entry.clone())?;
        let expected: LogRecord = tau_proto::decode_message_from_slice(&expected)
            .map_err(|_| LocatorFailure::Unavailable)?;
        if record.sequence != count
            || record.previous_hash != previous_hash
            || record.hash != expected.hash
        {
            return Err(LocatorFailure::Unavailable);
        }
        previous_hash = record.hash;
        match record.entry {
            DiskEntry::Located { key, record } => {
                if entries.insert(key.clone(), *record).is_some() {
                    ambiguous.insert(key);
                }
            }
            DiskEntry::Ambiguous { key } => {
                ambiguous.insert(key);
            }
        }
        count += 1;
    }
    Ok(LoadedLog {
        entries,
        ambiguous,
        head: LocatorHead {
            version: LOCATOR_SCHEMA_VERSION,
            count,
            log_bytes: size,
            last_hash: previous_hash,
        },
    })
}

/// Returns the deterministic rebuild ordering key for one disk entry.
pub(super) fn entry_sort_key(entry: &DiskEntry) -> (String, String, String) {
    let key = match entry {
        DiskEntry::Located { key, record: _ } | DiskEntry::Ambiguous { key } => key,
    };
    (
        key.extension_name.to_string(),
        key.transport_name.clone(),
        key.dedup_key.clone(),
    )
}

/// Reads one small bounded exact locator head.
pub(super) fn read_head(path: &Path) -> Result<LocatorHead, LocatorFailure> {
    let metadata = fs::metadata(path).map_err(|_| LocatorFailure::Unavailable)?;
    if metadata.len() > 4096 {
        return Err(LocatorFailure::Unavailable);
    }
    let bytes = fs::read(path).map_err(|_| LocatorFailure::Unavailable)?;
    tau_proto::decode_message_from_slice(&bytes).map_err(|_| LocatorFailure::Unavailable)
}

/// Atomically replaces and parent-syncs the locator head.
pub(super) fn write_head_atomic(path: &Path, head: &LocatorHead) -> io::Result<()> {
    let bytes = tau_proto::encode_message_to_vec(head).map_err(io::Error::other)?;
    write_atomic(path, &bytes)
}

/// Atomically replaces and parent-syncs one derived-state file.
pub(super) fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    write_and_sync(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    sync_parent(path)
}

/// Appends and syncs one length-framed locator record.
pub(super) fn append_framed(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(&(bytes.len() as u64).to_be_bytes())?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Replaces and syncs one file without publishing a rename.
pub(super) fn write_and_sync(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Removes a file and durably publishes its directory update.
pub(super) fn remove_and_sync_parent(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => sync_parent(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Syncs the parent directory containing a derived-state update.
pub(super) fn sync_parent(path: &Path) -> io::Result<()> {
    File::open(path.parent().unwrap_or_else(|| Path::new(".")))?.sync_all()
}

/// Checks path existence while preserving metadata errors.
pub(super) fn try_exists(path: &Path) -> io::Result<bool> {
    match fs::metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Counts complete framed records in an encoded rebuild buffer.
pub(super) fn count_records(bytes: &[u8]) -> Result<u64, LocatorFailure> {
    let mut cursor = io::Cursor::new(bytes);
    let mut count = 0;
    while cursor.position() < bytes.len() as u64 {
        let mut length = [0; 8];
        cursor
            .read_exact(&mut length)
            .map_err(|_| LocatorFailure::Durable)?;
        cursor
            .seek(SeekFrom::Current(u64::from_be_bytes(length) as i64))
            .map_err(|_| LocatorFailure::Durable)?;
        count += 1;
    }
    Ok(count)
}
