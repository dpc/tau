//! Private hardlink aliases keep transcript bytes safe from older CAS cleaners.
//!
//! Older binaries only clean `blake3`, `operations`, and `.cleanup`. They never
//! modify published data in place, so an ordinary object detach cannot destroy
//! this alias. These are the same objects/keys, not another public namespace.

use super::transcript_roots::{path_present, read_regular, require_directory};
use super::*;

#[cfg(test)]
mod tests;

impl ArtifactStore {
    /// Mirrors a new explicit put's age into any private alias, never on reads.
    pub(super) fn renew_transcript_object(
        &self,
        descriptor: &ArtifactDescriptor,
        put_at: u64,
    ) -> Result<(), ArtifactError> {
        let digest = self.storage_digest(&descriptor.key)?;
        let directory = self.root.join("transcript-objects").join(digest.as_hex());
        if !path_present(&directory)? {
            return Ok(());
        }
        require_directory(&directory)?;
        let mut metadata: Metadata = read_json(&directory.join("meta.json"))?;
        if metadata.descriptor(descriptor.key.clone())? != *descriptor {
            return Err(ArtifactError::Integrity);
        }
        metadata.last_put_at = metadata.last_put_at.max(put_at);
        replace_json(&directory.join("meta.json"), &metadata)
    }

    /// Selects the ordinary object first, using its protected alias only if
    /// absent.
    ///
    /// Corrupt ordinary metadata is an error, not authority to silently repair
    /// or overwrite it. Reads do not republish an expired ordinary catalog
    /// entry.
    pub(super) fn object_directory(
        &self,
        digest: &StorageDigest,
    ) -> Result<PathBuf, ArtifactError> {
        let ordinary = self.root.join("blake3").join(digest.as_hex());
        if path_present(&ordinary)? {
            return Ok(ordinary);
        }
        Ok(self.root.join("transcript-objects").join(digest.as_hex()))
    }

    /// Resolves a public identity to its full private storage digest.
    pub(super) fn storage_digest(&self, key: &ArtifactKey) -> Result<StorageDigest, ArtifactError> {
        self.resolve_short_digest(key)
    }

    /// Finds one private full-digest locator across both catalogs, never a
    /// first arbitrary prefix match. Duplicate hardlink aliases count as
    /// one object. The bounded scan needs no migration index or writes
    /// during reads.
    pub(super) fn resolve_short_digest(
        &self,
        key: &ArtifactKey,
    ) -> Result<StorageDigest, ArtifactError> {
        let started = Instant::now();
        let mut found = None;
        for catalog in ["blake3", "transcript-objects"] {
            let entries = match fs::read_dir(self.root.join(catalog)) {
                Ok(entries) => entries,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(io_error(error)),
            };
            for entry in entries {
                if started.elapsed() >= Duration::from_secs(2) {
                    return Err(ArtifactError::Busy);
                }
                let entry = entry.map_err(io_error)?;
                let name = entry.file_name();
                let Some(name) = name.to_str() else { continue };
                if !name.starts_with(key.as_str()) {
                    continue;
                }
                let Ok(digest) = StorageDigest::parse(name) else {
                    continue;
                };
                if found.as_ref().is_some_and(|previous| *previous != digest) {
                    return Err(ArtifactError::Integrity);
                }
                found = Some(digest);
            }
        }
        found.ok_or(ArtifactError::Unavailable)
    }

    /// Publishes and verifies a hardlink alias under the caller's exclusive
    /// lock.
    ///
    /// This must finish before root publication. Failure may leave an unrooted
    /// alias, but cannot acknowledge a root with only old-cleaner-vulnerable
    /// bytes.
    pub(super) fn protect_transcript_object(
        &self,
        descriptor: &ArtifactDescriptor,
    ) -> Result<(), ArtifactError> {
        let digest = self.storage_digest(&descriptor.key)?;
        let objects = self.root.join("transcript-objects");
        private_dir(&objects)?;
        require_directory(&objects)?;
        let protected = objects.join(digest.as_hex());
        if path_present(&protected)? {
            require_directory(&protected)?;
            let metadata: Metadata = read_json(&protected.join("meta.json"))?;
            if metadata.descriptor(descriptor.key.clone())? != *descriptor {
                return Err(ArtifactError::Integrity);
            }
            verify(
                descriptor,
                &read_regular(&protected.join("data"), descriptor.size.get())?,
            )?;
        } else {
            let ordinary = self.root.join("blake3").join(digest.as_hex());
            require_directory(&ordinary)?;
            verify(
                descriptor,
                &read_regular(&ordinary.join("data"), descriptor.size.get())?,
            )?;
            let staging = tempfile::Builder::new()
                .prefix(".staging-")
                .tempdir_in(&objects)
                .map_err(io_error)?;
            fs::set_permissions(staging.path(), fs::Permissions::from_mode(0o700))
                .map_err(io_error)?;
            // Hardlink failure rejects admission; never silently copy or weaken
            // durability. Both paths are in the same selected CAS filesystem.
            fs::hard_link(ordinary.join("data"), staging.path().join("data")).map_err(io_error)?;
            File::open(staging.path().join("data"))
                .and_then(|file| file.sync_all())
                .map_err(io_error)?;
            write_json(
                &staging.path().join("meta.json"),
                &self.metadata(&descriptor.key)?,
            )?;
            sync_dir(staging.path())?;
            fs::rename(staging.path(), &protected).map_err(io_error)?;
        }
        // Retried publication repairs any earlier parent-sync failure.
        sync_dir(&protected)?;
        sync_dir(&objects)?;
        sync_dir(&self.root)
    }
}
