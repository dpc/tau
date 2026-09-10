//! Shared immutable originals and retry receipts, outside semantic persistence.
//!
//! `SPEC-shared-artifacts` governs the cross-component contract. All operations
//! run on the artifact worker, never on the event publication owner.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fs2::FileExt;
use tau_proto::{
    ARTIFACT_CHUNK_BYTES, ARTIFACT_MAX_BYTES, ArtifactDescriptor, ArtifactError, ArtifactKey,
    ArtifactOp, ArtifactReadId, ArtifactUploadId, ArtifactValue, artifact_digest,
    artifact_identifier,
};

mod cleanup;
mod filesystem;
mod metadata;
mod reader;
mod receipt;
#[cfg(test)]
mod tests;
mod upload;

use filesystem::*;
use metadata::Metadata;
use reader::Reader;
use receipt::Receipt;
use upload::Upload;

/// Maximum simultaneously open uploads and reads in one harness.
const TRANSFER_LIMIT: usize = 8;
/// Non-renewable transfer lifetime; chunk traffic cannot extend it.
const TRANSFER_LIFETIME: Duration = Duration::from_secs(120);
/// Completed or interrupted finalization receipts remain retryable for one day.
const RECEIPT_LIFETIME: u64 = 24 * 60 * 60;

/// Worker-owned state; construction itself performs no filesystem access.
pub(crate) struct ArtifactStore {
    /// Independent shared domain beneath the selected persistent state root.
    root: PathBuf,
    /// Incomplete connection-owned uploads; disconnect/timeout drops bytes.
    uploads: HashMap<ArtifactUploadId, Upload>,
    /// Bounded original readers holding shared cross-process coordination.
    reads: HashMap<ArtifactReadId, Reader>,
}

impl ArtifactStore {
    /// Constructs inert worker state; memory-only admission never constructs
    /// it.
    pub(crate) fn new(state_root: &Path) -> Self {
        Self {
            root: state_root.join("artifacts"),
            uploads: HashMap::new(),
            reads: HashMap::new(),
        }
    }

    /// Drops abandoned connection state; committed objects and receipts
    /// survive.
    pub(crate) fn disconnect(&mut self, connection: &str) {
        self.uploads
            .retain(|_, upload| upload.connection != connection);
        self.reads.retain(|_, read| read.connection != connection);
    }

    /// Releases expired transfers even when no peer sends another request.
    pub(crate) fn expire(&mut self, now: Instant) {
        self.uploads.retain(|_, upload| upload.expires > now);
        self.reads.retain(|_, read| read.expires > now);
    }

    /// Executes one bounded RPC outside the harness loop.
    pub(crate) fn execute(
        &mut self,
        owner: &str,
        connection: &str,
        op: ArtifactOp,
        now: u64,
    ) -> Result<ArtifactValue, ArtifactError> {
        self.expire(Instant::now());
        match op {
            ArtifactOp::Available => Ok(ArtifactValue::Done),
            ArtifactOp::Begin { size } => self.begin(owner, connection, size),
            ArtifactOp::Write {
                upload,
                offset,
                bytes,
            } => self.write(connection, &upload, offset, &bytes),
            ArtifactOp::Finalize { upload } => self.finalize(owner, connection, &upload, now),
            ArtifactOp::Abort { upload } => {
                if let Some(active) = self.uploads.get(&upload)
                    && active.connection != connection
                {
                    return Err(ArtifactError::Permission);
                }
                self.uploads.remove(&upload);
                // Once finalization has an intent, it may have committed. Abort
                // never removes an intent, receipt, or shared original.
                Ok(ArtifactValue::Done)
            }
            ArtifactOp::Stat { key } => {
                let _lock = self.lock(false)?;
                Ok(ArtifactValue::Descriptor(self.descriptor(&key)?))
            }
            ArtifactOp::Open { key } => self.open(connection, key),
            ArtifactOp::Read {
                read,
                offset,
                length,
            } => self.read(connection, &read, offset, length),
            ArtifactOp::Close { read } => {
                if let Some(active) = self.reads.get(&read)
                    && active.connection != connection
                {
                    return Err(ArtifactError::Permission);
                }
                self.reads.remove(&read);
                Ok(ArtifactValue::Done)
            }
        }
    }

    fn begin(
        &mut self,
        owner: &str,
        connection: &str,
        size: tau_proto::ArtifactSize,
    ) -> Result<ArtifactValue, ArtifactError> {
        if self.uploads.len() + self.reads.len() >= TRANSFER_LIMIT {
            return Err(ArtifactError::Busy);
        }
        let upload = ArtifactUploadId::parse(new_id())?;
        self.uploads.insert(
            upload.clone(),
            Upload {
                owner: owner.to_owned(),
                connection: connection.to_owned(),
                size,
                bytes: Vec::new(),
                expires: Instant::now() + TRANSFER_LIFETIME,
            },
        );
        Ok(ArtifactValue::Upload { upload })
    }

    fn write(
        &mut self,
        connection: &str,
        id: &ArtifactUploadId,
        offset: u64,
        bytes: &[u8],
    ) -> Result<ArtifactValue, ArtifactError> {
        if bytes.len() > ARTIFACT_CHUNK_BYTES {
            return Err(ArtifactError::Invalid);
        }
        let upload = self.uploads.get_mut(id).ok_or(ArtifactError::Unavailable)?;
        if upload.connection != connection {
            return Err(ArtifactError::Permission);
        }
        let end = offset
            .checked_add(bytes.len() as u64)
            .ok_or(ArtifactError::Invalid)?;
        if end > upload.size.get() || offset > upload.bytes.len() as u64 {
            return Err(ArtifactError::Invalid);
        }
        if offset == upload.bytes.len() as u64 {
            upload.bytes.extend_from_slice(bytes);
        } else if upload.bytes.get(offset as usize..end as usize) != Some(bytes) {
            return Err(ArtifactError::Integrity);
        }
        Ok(ArtifactValue::Written {
            next_offset: upload.bytes.len() as u64,
        })
    }

    fn finalize(
        &mut self,
        owner: &str,
        connection: &str,
        id: &ArtifactUploadId,
        now: u64,
    ) -> Result<ArtifactValue, ArtifactError> {
        let _lock = self.lock(true)?;
        let receipt_path = self.root.join("operations").join(id.as_str());
        let mut receipt = if receipt_path.exists() {
            let receipt: Receipt = read_json(&receipt_path.join("receipt.json"))?;
            if receipt.version != 1 || receipt.owner != owner {
                return Err(ArtifactError::Permission);
            }
            if now >= receipt.expires_at {
                return Err(ArtifactError::Unavailable);
            }
            receipt
        } else {
            let upload = self.uploads.get(id).ok_or(ArtifactError::Unavailable)?;
            if upload.connection != connection || upload.owner != owner {
                return Err(ArtifactError::Permission);
            }
            if upload.bytes.len() as u64 != upload.size.get() {
                return Err(ArtifactError::Invalid);
            }
            let receipt = Receipt {
                version: 1,
                owner: owner.to_owned(),
                descriptor: ArtifactDescriptor::new(
                    ArtifactKey::parse(format!("blake3:{}", blake3::hash(&upload.bytes).to_hex()))?,
                    upload.size.get(),
                )?,
                put_at: now,
                expires_at: now.saturating_add(RECEIPT_LIFETIME),
                complete: false,
            };
            let staging = tempfile::Builder::new()
                .prefix(".staging-")
                .tempdir_in(self.root.join("operations"))
                .map_err(io_error)?;
            fs::set_permissions(staging.path(), fs::Permissions::from_mode(0o700))
                .map_err(io_error)?;
            write_synced(&staging.path().join("data"), &upload.bytes)?;
            write_json(&staging.path().join("receipt.json"), &receipt)?;
            sync_dir(staging.path())?;
            fs::rename(staging.path(), &receipt_path).map_err(io_error)?;
            sync_dir(&self.root.join("operations"))?;
            receipt
        };
        // A previous rename may have succeeded while its directory sync failed.
        // Re-establish the intent boundary before any retry mutates shared age.
        sync_dir(&receipt_path)?;
        sync_dir(&self.root.join("operations"))?;
        if !receipt.complete {
            self.publish_receipt(&receipt_path, &receipt)?;
            receipt.complete = true;
            replace_json(&receipt_path.join("receipt.json"), &receipt)?;
        }
        // A response-lost retry also repairs a previous receipt-parent sync
        // error.
        sync_dir(&receipt_path)?;
        // Data removal is recovery housekeeping, never a precondition for
        // success.
        let _ = fs::remove_file(receipt_path.join("data"));
        self.uploads.remove(id);
        Ok(ArtifactValue::Descriptor(receipt.descriptor))
    }

    fn publish_receipt(&self, receipt_path: &Path, receipt: &Receipt) -> Result<(), ArtifactError> {
        let key = &receipt.descriptor.key;
        let hex = artifact_digest(key).ok_or(ArtifactError::Integrity)?;
        let objects = self.root.join("blake3");
        let object = objects.join(hex);
        let data = read_bounded(&receipt_path.join("data"))?;
        verify(&receipt.descriptor, &data)?;
        if object.exists() {
            let old = self.metadata(key)?;
            if old.size != receipt.descriptor.size.get() {
                return Err(ArtifactError::Integrity);
            }
            verify(&receipt.descriptor, &read_bounded(&object.join("data"))?)?;
            replace_json(
                &object.join("meta.json"),
                &Metadata {
                    version: 1,
                    size: receipt.descriptor.size.get(),
                    last_put_at: old.last_put_at.max(receipt.put_at),
                },
            )?;
        } else {
            let staging = tempfile::Builder::new()
                .prefix(".staging-")
                .tempdir_in(&objects)
                .map_err(io_error)?;
            fs::set_permissions(staging.path(), fs::Permissions::from_mode(0o700))
                .map_err(io_error)?;
            write_synced(&staging.path().join("data"), &data)?;
            write_json(
                &staging.path().join("meta.json"),
                &Metadata {
                    version: 1,
                    size: receipt.descriptor.size.get(),
                    last_put_at: receipt.put_at,
                },
            )?;
            sync_dir(staging.path())?;
            fs::rename(staging.path(), &object).map_err(io_error)?;
            sync_dir(&objects)?;
        }
        // Also repairs an earlier object rename whose parent sync failed before
        // this retry took the existing-object branch.
        sync_dir(&objects)
    }

    fn metadata(&self, key: &ArtifactKey) -> Result<Metadata, ArtifactError> {
        let hex = artifact_digest(key).ok_or(ArtifactError::Invalid)?;
        let metadata: Metadata = read_json(&self.root.join("blake3").join(hex).join("meta.json"))?;
        if metadata.version != 1 || metadata.size > ARTIFACT_MAX_BYTES {
            return Err(ArtifactError::Integrity);
        }
        Ok(metadata)
    }

    fn descriptor(&self, key: &ArtifactKey) -> Result<ArtifactDescriptor, ArtifactError> {
        ArtifactDescriptor::new(key.clone(), self.metadata(key)?.size)
    }

    fn open(&mut self, connection: &str, key: ArtifactKey) -> Result<ArtifactValue, ArtifactError> {
        if self.uploads.len() + self.reads.len() >= TRANSFER_LIMIT {
            return Err(ArtifactError::Busy);
        }
        let lock = self.lock(false)?;
        let descriptor = self.descriptor(&key)?;
        let hex = artifact_digest(&key).ok_or(ArtifactError::Invalid)?;
        let data = File::open(self.root.join("blake3").join(hex).join("data")).map_err(io_error)?;
        if data.metadata().map_err(io_error)?.len() != descriptor.size.get() {
            return Err(ArtifactError::Integrity);
        }
        let read = ArtifactReadId::parse(new_id())?;
        self.reads.insert(
            read.clone(),
            Reader {
                connection: connection.to_owned(),
                data,
                _lock: lock,
                size: descriptor.size.get(),
                expires: Instant::now() + TRANSFER_LIFETIME,
            },
        );
        Ok(ArtifactValue::Opened { read, descriptor })
    }

    fn read(
        &mut self,
        connection: &str,
        id: &ArtifactReadId,
        offset: u64,
        length: u32,
    ) -> Result<ArtifactValue, ArtifactError> {
        if length as usize > ARTIFACT_CHUNK_BYTES || length == 0 {
            return Err(ArtifactError::Invalid);
        }
        let read = self.reads.get_mut(id).ok_or(ArtifactError::Unavailable)?;
        if read.connection != connection {
            return Err(ArtifactError::Permission);
        }
        if offset > read.size {
            return Err(ArtifactError::Invalid);
        }
        let mut bytes = vec![0; (read.size - offset).min(u64::from(length)) as usize];
        read.data.seek(SeekFrom::Start(offset)).map_err(io_error)?;
        read.data.read_exact(&mut bytes).map_err(io_error)?;
        let eof = offset + bytes.len() as u64 == read.size;
        // Explicit Close preserves exact last-chunk retransmission after
        // response loss.
        Ok(ArtifactValue::Chunk { offset, bytes, eof })
    }

    fn lock(&self, exclusive: bool) -> Result<File, ArtifactError> {
        private_dir(&self.root)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(self.root.join(".lock"))
            .map_err(io_error)?;
        let locked = if exclusive {
            FileExt::try_lock_exclusive(&lock)
        } else {
            FileExt::try_lock_shared(&lock)
        };
        locked.map_err(|error| {
            if error.kind() == io::ErrorKind::WouldBlock {
                ArtifactError::Busy
            } else {
                io_error(error)
            }
        })?;
        for name in ["blake3", "operations", ".cleanup"] {
            private_dir(&self.root.join(name))?;
        }
        sync_dir(&self.root)?;
        // Sync the domain entry as well as every successful publication parent.
        sync_dir(self.root.parent().ok_or(ArtifactError::Io)?)?;
        Ok(lock)
    }
}
