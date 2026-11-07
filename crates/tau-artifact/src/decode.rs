//! Narrow tar extraction into a private staging directory, published only on
//! success.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Cursor, Read};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

use zstd::stream::read::Decoder;

use crate::bounded_io::BoundedIo;
use crate::{Limits, MAGIC, MAX_ENTRIES, VERSION, check_cancel, check_path, invalid};

/// Validates and extracts a complete verified bundle into an owned private
/// temp.
///
/// Dropping the returned owner cleans the tree; the caller retains it only
/// after successfully reporting the path. No permissions, links, or owners are
/// restored.
pub fn unpack(
    bytes: &[u8],
    limits: Limits,
    cancelled: impl Fn() -> bool,
) -> io::Result<tempfile::TempDir> {
    if bytes.len() as u64 > limits.stored_bytes
        || !bytes.starts_with(MAGIC)
        || bytes.get(MAGIC.len()) != Some(&VERSION)
    {
        return Err(invalid("invalid or unsupported Tau directory bundle"));
    }
    let payload = &bytes[MAGIC.len() + 1..];
    let mut decoder = Decoder::with_buffer(Cursor::new(payload))?.single_frame();
    decoder.window_log_max(23)?;
    let bounded = BoundedIo {
        inner: decoder,
        remaining: limits.stream_bytes(),
        cancelled: &cancelled,
    };
    let mut archive = tar::Archive::new(bounded);
    let temp = tempfile::Builder::new()
        .prefix("tau-directory-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let mut seen = HashSet::new();
    let mut remaining = limits.expanded_bytes;
    for entry in archive.entries()?.raw(true) {
        check_cancel(&cancelled)?;
        let mut entry = entry?;
        let path = std::str::from_utf8(&entry.path_bytes())
            .map_err(|_| invalid("non-UTF-8 directory path"))?
            .to_owned();
        check_path(&path)?;
        if seen.len() >= MAX_ENTRIES || !seen.insert(path.clone()) {
            return Err(invalid("duplicate or excessive directory members"));
        }
        let target = temp.path().join(&path);
        // Parents must already exist as explicit directory members. Only this
        // worker mutates the newly created private staging tree.
        let kind = entry.header().entry_type();
        if kind == tar::EntryType::Directory && entry.size() == 0 {
            fs::DirBuilder::new().mode(0o700).create(target)?;
        } else if kind == tar::EntryType::Regular {
            remaining = remaining
                .checked_sub(entry.size())
                .ok_or_else(|| invalid("expanded directory exceeds byte limit"))?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(target)?;
            let expected = entry.size();
            let mut reader = BoundedIo {
                inner: &mut entry,
                remaining: expected,
                cancelled: &cancelled,
            };
            if io::copy(&mut reader, &mut file)? != expected {
                return Err(invalid("truncated directory member"));
            }
        } else {
            return Err(invalid("unsupported directory entry type"));
        }
    }
    // Tar stops at its first zero header. Consume the full frame to validate
    // its checksum/epilogue and refuse hidden nonzero payload after the
    // archive.
    let mut bounded = archive.into_inner();
    let mut trailing = 0;
    let mut buffer = [0u8; 8192];
    loop {
        let count = bounded.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        if buffer[..count].iter().any(|byte| *byte != 0) {
            return Err(invalid("nonzero bytes after directory tar end"));
        }
        trailing += count;
    }
    if trailing < 512 || bounded.inner.finish().position() != payload.len() as u64 {
        return Err(invalid("incomplete tar end or trailing compressed bytes"));
    }
    check_cancel(&cancelled)?;
    Ok(temp)
}
