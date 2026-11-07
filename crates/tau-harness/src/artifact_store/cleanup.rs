//! Opportunistic, cross-process-coordinated artifact retention and recovery.

use super::*;

/// Bound work in a single startup maintenance pass.
const CLEANUP_LIMIT: usize = 1024;

impl ArtifactStore {
    /// Runs a bounded opportunistic cleanup pass; busy readers skip this pass.
    ///
    /// Call only in persistent mode. Missing roots remain untouched. Unknown or
    /// malformed entries are preserved and counted as failures.
    #[cfg(test)]
    pub(crate) fn cleanup(
        &self,
        retention: Option<Duration>,
        now: u64,
    ) -> Result<(), ArtifactError> {
        self.cleanup_reporting(retention, now, |_| {})
    }

    /// Runs cleanup while reporting each corrupt bookkeeping file that was
    /// preserved so startup can surface a nonfatal warning.
    pub(crate) fn cleanup_reporting(
        &self,
        retention: Option<Duration>,
        now: u64,
        mut report_corrupt: impl FnMut(&Path),
    ) -> Result<(), ArtifactError> {
        if !self.root.exists() {
            return Ok(());
        }
        let _lock = self.lock(true)?;
        let mut found_corruption = false;
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
        // ordinary textual references never pin original bytes.
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
                let receipt_path = entry.path().join("receipt.json");
                let receipt: Receipt = match read_json::<Receipt>(&receipt_path) {
                    Ok(receipt) if receipt.version == 1 => receipt,
                    Ok(_) | Err(ArtifactError::Integrity) => {
                        found_corruption = true;
                        report_corrupt(&receipt_path);
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                if now >= receipt.expires_at {
                    self.detach(&entry.path())?;
                }
            }
        }
        let mut entries = fs::read_dir(self.root.join("blake3"))
            .map_err(io_error)?
            .take(CLEANUP_LIMIT)
            .collect::<Result<Vec<_>, _>>()
            .map_err(io_error)?;
        match fs::read_dir(self.root.join("transcript-objects")) {
            Ok(protected) => entries.extend(
                protected
                    .take(CLEANUP_LIMIT)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(io_error)?,
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error(error)),
        }
        let candidates = entries
            .iter()
            .filter_map(|entry| ArtifactKey::from_digest(&entry.file_name().to_string_lossy()).ok())
            .collect();
        // Never detach from a partial root scan. This includes malformed or
        // missing manifests and exhausted scan budgets.
        let retained = self.retained_transcript_artifacts(&candidates, &mut report_corrupt)?;
        for entry in entries {
            if !entry.file_type().map_err(io_error)?.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(".staging-") {
                self.detach(&entry.path())?;
                continue;
            }
            let Ok(key) = ArtifactKey::from_digest(&name) else {
                continue;
            };
            if retained.contains(&key) {
                continue;
            }
            let metadata_path = entry.path().join("meta.json");
            let metadata = match self.metadata_at_digest(&name) {
                Ok(metadata) => metadata,
                Err(ArtifactError::Integrity) => {
                    found_corruption = true;
                    report_corrupt(&metadata_path);
                    continue;
                }
                Err(error) => return Err(error),
            };
            if retention.is_some_and(|age| {
                now.checked_sub(metadata.last_put_at)
                    .is_some_and(|elapsed| elapsed >= age.as_secs())
            }) {
                // Age was read under the same stable exclusive lock as renewal
                // and detach, not from a stale enumeration-time observation.
                self.detach(&entry.path())?;
            }
        }
        if found_corruption {
            Err(ArtifactError::Integrity)
        } else {
            Ok(())
        }
    }

    pub(super) fn detach(&self, path: &Path) -> Result<(), ArtifactError> {
        let cleanup = self.root.join(".cleanup");
        let detached = cleanup.join(new_id());
        fs::rename(path, &detached).map_err(io_error)?;
        sync_dir(path.parent().ok_or(ArtifactError::Io)?)?;
        sync_dir(&cleanup)?;
        fs::remove_dir_all(detached).map_err(io_error)?;
        sync_dir(&cleanup)
    }
}
