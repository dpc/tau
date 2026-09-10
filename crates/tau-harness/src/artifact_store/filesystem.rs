//! Bounded private-file primitives used only by the artifact worker.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;

use serde::Serialize;
use tau_proto::{ARTIFACT_MAX_BYTES, ArtifactDescriptor, ArtifactError, artifact_digest};

pub(super) fn new_id() -> String {
    format!("{:032x}", rand::random::<u128>())
}

pub(super) fn private_dir(path: &Path) -> Result<(), ArtifactError> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(io_error)
}

pub(super) fn io_error(error: io::Error) -> ArtifactError {
    if error.kind() == io::ErrorKind::NotFound {
        ArtifactError::Unavailable
    } else {
        ArtifactError::Io
    }
}

pub(super) fn sync_dir(path: &Path) -> Result<(), ArtifactError> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(io_error)
}

pub(super) fn write_synced(path: &Path, bytes: &[u8]) -> Result<(), ArtifactError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(io_error)?;
    file.write_all(bytes).map_err(io_error)?;
    file.sync_all().map_err(io_error)
}

pub(super) fn write_json(path: &Path, value: &impl Serialize) -> Result<(), ArtifactError> {
    write_synced(
        path,
        &serde_json::to_vec(value).map_err(|_| ArtifactError::Integrity)?,
    )
}

pub(super) fn replace_json(path: &Path, value: &impl Serialize) -> Result<(), ArtifactError> {
    let parent = path.parent().ok_or(ArtifactError::Io)?;
    // One fixed temporary per record directory bounds interrupted replacements.
    // The stable store lock serializes writers and cleanup across processes.
    let next = parent.join(".metadata-next");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&next)
        .map_err(io_error)?;
    serde_json::to_writer(&mut file, value).map_err(|_| ArtifactError::Io)?;
    file.sync_all().map_err(io_error)?;
    fs::rename(next, path).map_err(io_error)?;
    sync_dir(parent)
}

pub(super) fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, ArtifactError> {
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(io_error)?
        .take(4097)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() > 4096 {
        return Err(ArtifactError::Integrity);
    }
    serde_json::from_slice(&bytes).map_err(|_| ArtifactError::Integrity)
}

pub(super) fn read_bounded(path: &Path) -> Result<Vec<u8>, ArtifactError> {
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(io_error)?
        .take(ARTIFACT_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() as u64 > ARTIFACT_MAX_BYTES {
        return Err(ArtifactError::Integrity);
    }
    Ok(bytes)
}

pub(super) fn verify(descriptor: &ArtifactDescriptor, bytes: &[u8]) -> Result<(), ArtifactError> {
    if descriptor.size.get() != bytes.len() as u64
        || artifact_digest(&descriptor.key) != Some(blake3::hash(bytes).to_hex().as_str())
    {
        return Err(ArtifactError::Integrity);
    }
    Ok(())
}
