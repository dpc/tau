//! Opportunistic, cross-process-coordinated artifact retention and recovery.

use super::*;

/// Bound work in a single startup maintenance pass.
const CLEANUP_LIMIT: usize = 1024;

impl ArtifactStore {
    /// Runs a bounded opportunistic cleanup pass; busy readers skip this pass.
    ///
    /// Call only in persistent mode. Missing roots remain untouched. Unknown or
    /// malformed entries are preserved and counted as failures.
    pub(crate) fn cleanup(
        &self,
        retention: Option<Duration>,
        now: u64,
    ) -> Result<(), ArtifactError> {
        if !self.root.exists() {
            return Ok(());
        }
        let _lock = self.lock(true)?;
        let cleanup = self.root.join(".cleanup");
        // Recover a detach whose process may have died before parent sync.
        sync_dir(&self.root.join("operations"))?;
        sync_dir(&self.root.join("blake3"))?;
        sync_dir(&cleanup)?;
        for entry in fs::read_dir(&cleanup)
            .map_err(io_error)?
            .take(CLEANUP_LIMIT)
        {
            let entry = entry.map_err(io_error)?;
            if entry.file_type().map_err(io_error)?.is_dir()
                && artifact_identifier(&entry.file_name().to_string_lossy())
            {
                fs::remove_dir_all(entry.path()).map_err(io_error)?;
                sync_dir(&cleanup)?;
            }
        }
        // Intents and receipts have their own finite recovery lifetime;
        // object references never pin original bytes.
        for entry in fs::read_dir(self.root.join("operations"))
            .map_err(io_error)?
            .take(CLEANUP_LIMIT)
        {
            let entry = entry.map_err(io_error)?;
            if !entry.file_type().map_err(io_error)?.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(".staging-") {
                self.detach(&entry.path())?;
            } else if artifact_identifier(&name) {
                let receipt: Receipt = read_json(&entry.path().join("receipt.json"))?;
                if receipt.version == 1 && now >= receipt.expires_at {
                    self.detach(&entry.path())?;
                }
            }
        }
        for entry in fs::read_dir(self.root.join("blake3"))
            .map_err(io_error)?
            .take(CLEANUP_LIMIT)
        {
            let entry = entry.map_err(io_error)?;
            if !entry.file_type().map_err(io_error)?.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(".staging-") {
                self.detach(&entry.path())?;
                continue;
            }
            let Ok(key) = ArtifactKey::parse(format!("blake3:{name}")) else {
                continue;
            };
            let metadata = self.metadata(&key)?;
            if retention.is_some_and(|age| {
                now.checked_sub(metadata.last_put_at)
                    .is_some_and(|elapsed| elapsed >= age.as_secs())
            }) {
                // Age was read under the same stable exclusive lock as renewal
                // and detach, not from a stale enumeration-time observation.
                self.detach(&entry.path())?;
            }
        }
        Ok(())
    }

    fn detach(&self, path: &Path) -> Result<(), ArtifactError> {
        let cleanup = self.root.join(".cleanup");
        let detached = cleanup.join(new_id());
        fs::rename(path, &detached).map_err(io_error)?;
        sync_dir(path.parent().ok_or(ArtifactError::Io)?)?;
        sync_dir(&cleanup)?;
        fs::remove_dir_all(detached).map_err(io_error)?;
        sync_dir(&cleanup)
    }
}
