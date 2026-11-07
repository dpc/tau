//! Advisory bounded-memory discovery on the sole artifact filesystem worker.

use tau_proto::{ARTIFACT_LIST_LIMIT, ArtifactListEntry, artifact_prefix_valid};

use super::*;

/// Total scan decision budget, checked between filesystem operations.
const SCAN_BUDGET: Duration = Duration::from_secs(2);

impl ArtifactStore {
    /// Returns a complete top-N answer or an error, never a misleading partial
    /// page.
    ///
    /// This advisory scan intentionally holds no retention lock. Atomic
    /// metadata replacement makes individual observations coherent; puts
    /// and cleanup may race the overall page. Filesystem syscalls
    /// themselves are not interruptible.
    pub(crate) fn list(
        &self,
        prefix: &str,
        limit: u8,
        alive: impl Fn() -> bool,
    ) -> Result<ArtifactValue, ArtifactError> {
        if !artifact_prefix_valid(prefix) || limit == 0 || ARTIFACT_LIST_LIMIT < limit {
            return Err(ArtifactError::Invalid);
        }
        let started = Instant::now();
        let check = || {
            if !alive() || started.elapsed() >= SCAN_BUDGET {
                Err(ArtifactError::Busy)
            } else {
                Ok(())
            }
        };
        check()?;
        let directory = match fs::read_dir(self.root.join("blake3")) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(ArtifactValue::Listed { entries: vec![] });
            }
            Err(error) => return Err(io_error(error)),
        };
        let mut entries: Vec<ArtifactListEntry> = Vec::with_capacity(limit as usize + 1);
        for entry in directory {
            check()?;
            let entry = entry.map_err(io_error)?;
            if !entry.file_type().map_err(io_error)?.is_dir() {
                continue;
            }
            let Ok(key) =
                ArtifactKey::parse(format!("blake3:{}", entry.file_name().to_string_lossy()))
            else {
                continue;
            };
            // "b" is a canonical hex prefix, not an abbreviation for the old
            // algorithm label. Longer label prefixes remain transition input.
            let spelling = if prefix.starts_with("bl") {
                key.as_str()
            } else {
                key.canonical()
            };
            if !spelling.starts_with(prefix) {
                continue;
            }
            let metadata = match self.metadata(&key) {
                Ok(metadata) => metadata,
                Err(ArtifactError::Unavailable | ArtifactError::Integrity) => continue,
                Err(error) => return Err(error),
            };
            entries.push(ArtifactListEntry {
                descriptor: ArtifactDescriptor::new(
                    ArtifactKey::parse(key.canonical())?,
                    metadata.size,
                )?,
                last_put_at: metadata.last_put_at,
                signature: None,
            });
            entries.sort_unstable_by(|a, b| {
                b.last_put_at
                    .cmp(&a.last_put_at)
                    .then_with(|| a.descriptor.key.as_str().cmp(b.descriptor.key.as_str()))
            });
            entries.truncate(limit as usize);
        }
        for entry in &mut entries {
            check()?;
            let digest = self.storage_digest(&entry.descriptor.key)?;
            if let Ok(mut file) = File::open(self.root.join("blake3").join(digest).join("data")) {
                let mut header = [0_u8; 32];
                if let Ok(length) = file.read(&mut header) {
                    entry.signature = tau_client::artifact_signature(&header[..length]);
                }
            }
        }
        check()?;
        Ok(ArtifactValue::Listed { entries })
    }
}
