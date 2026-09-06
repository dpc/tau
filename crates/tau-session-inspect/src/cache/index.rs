//! Explicit owner-private disposable cache geometry index.

use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind, Read as _, Write};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use serde::{Deserialize, Serialize};

use super::exact_geometry::{ExactRequest, ExactResponse, FingerprintKey};

#[cfg(test)]
mod tests;

/// Loaded index key and fixed-size historical evidence.
pub(super) struct IndexState {
    /// Requested destination.
    path: PathBuf,
    /// Secret retained only in this index and process memory.
    pub(super) key: FingerprintKey,
    /// Previously indexed requests.
    pub(super) requests: Vec<ExactRequest>,
    /// Previously indexed successful-response identities.
    pub(super) responses: Vec<ExactResponse>,
    /// Maximum encoded index bytes.
    limit: u64,
}

/// Closed disposable on-disk representation.
#[derive(Deserialize, Serialize)]
struct IndexFile {
    /// Closed index schema.
    schema: String,
    /// Internal schema version.
    schema_version: u64,
    /// Matching executable build identity.
    producer_build: String,
    /// Base64 inspection key; never emitted in reports.
    key: String,
    /// Fixed-size request evidence.
    requests: Vec<ExactRequest>,
    /// Fixed-size response identity evidence.
    responses: Vec<ExactResponse>,
}

impl IndexState {
    /// Loads a valid existing index or creates a fresh in-memory key.
    pub(super) fn open(
        path: &Path,
        producer_build: &str,
        working_memory_bytes: u64,
    ) -> Result<Self, &'static str> {
        let limit = working_memory_bytes / 4;
        if path
            .symlink_metadata()
            .is_err_and(|error| error.kind() == ErrorKind::NotFound)
        {
            return Ok(Self {
                path: path.to_owned(),
                key: FingerprintKey::random()?,
                requests: Vec::new(),
                responses: Vec::new(),
                limit,
            });
        }
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = options.open(path).map_err(|_| "cache_index_unreadable")?;
        validate_private(&file, path)?;
        let metadata = file.metadata().map_err(|_| "cache_index_unreadable")?;
        if metadata.len() > limit {
            return Err("cache_index_memory_limit");
        }
        let mut bytes = Vec::new();
        file.take(limit.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|_| "cache_index_unreadable")?;
        if bytes.len() as u64 > limit {
            return Err("cache_index_memory_limit");
        }
        let value = serde_json::from_slice::<super::strict_json::StrictJson>(&bytes)
            .map_err(|_| "cache_index_malformed")?
            .0;
        let mut index: IndexFile =
            serde_json::from_value(value).map_err(|_| "cache_index_malformed")?;
        if index.schema != "tau.cache_diagnostic.index"
            || index.schema_version != 0
            || index.producer_build != producer_build
        {
            return Err("cache_index_unsupported_schema_or_build");
        }
        let decoded = STANDARD_NO_PAD
            .decode(&index.key)
            .map_err(|_| "cache_index_malformed")?;
        let key: [u8; 32] = decoded.try_into().map_err(|_| "cache_index_malformed")?;
        for request in &mut index.requests {
            if !super::exact_geometry::selection_metadata_valid(request) {
                return Err("cache_index_malformed");
            }
            request.indexed = true;
        }
        for response in &mut index.responses {
            response.indexed = true;
        }
        Ok(Self {
            path: path.to_owned(),
            key: FingerprintKey(key),
            requests: index.requests,
            responses: index.responses,
            limit,
        })
    }

    /// Atomically replaces the requested index with complete bounded evidence.
    pub(super) fn commit(
        &self,
        producer_build: &str,
        requests: &[ExactRequest],
        responses: &[ExactResponse],
    ) -> Result<(), &'static str> {
        if requests
            .iter()
            .any(|request| !super::exact_geometry::selection_metadata_valid(request))
        {
            return Err("cache_index_malformed");
        }
        let index = IndexFile {
            schema: "tau.cache_diagnostic.index".to_owned(),
            schema_version: 0,
            producer_build: producer_build.to_owned(),
            key: STANDARD_NO_PAD.encode(self.key.0),
            requests: requests.to_vec(),
            responses: responses.to_vec(),
        };
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        let mut temp = tempfile::Builder::new()
            .prefix(".tau-cache-index-")
            .tempfile_in(parent)
            .map_err(|_| "cache_index_temp_create_failed")?;
        let mut bounded = BoundedWriter::new(temp.as_file_mut(), self.limit);
        serde_json::to_writer(&mut bounded, &index).map_err(|error| {
            if error.io_error_kind() == Some(ErrorKind::WriteZero) {
                "cache_index_memory_limit"
            } else {
                "cache_index_write_failed"
            }
        })?;
        bounded.flush().map_err(|_| "cache_index_write_failed")?;
        temp.persist(&self.path)
            .map_err(|_| "cache_index_replace_failed")?;
        Ok(())
    }
}

/// Ensures a reused key was not read from a shared or substituted file.
fn validate_private(file: &File, _path: &Path) -> Result<(), &'static str> {
    let metadata = file.metadata().map_err(|_| "cache_index_unreadable")?;
    if !metadata.is_file() {
        return Err("cache_index_not_regular");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if metadata.uid() != rustix_v1::process::geteuid().as_raw() {
            return Err("cache_index_wrong_owner");
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err("cache_index_not_private");
        }
    }
    Ok(())
}

/// Writer which rejects byte `limit + 1` without publishing a partial index.
struct BoundedWriter<'a> {
    /// Sibling temporary file.
    output: &'a mut File,
    /// Inclusive byte limit.
    remaining: u64,
}

impl<'a> BoundedWriter<'a> {
    /// Constructs a bounded file adapter.
    fn new(output: &'a mut File, limit: u64) -> Self {
        Self {
            output,
            remaining: limit,
        }
    }
}

impl Write for BoundedWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() as u64 > self.remaining {
            return Err(io::Error::new(
                ErrorKind::WriteZero,
                "cache index byte limit exceeded",
            ));
        }
        let written = self.output.write(bytes)?;
        self.remaining = self.remaining.saturating_sub(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}
