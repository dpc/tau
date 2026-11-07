//! Sorted fd-relative traversal with no symlink following.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;

use rustix_v1::fs::{Dir, Mode, OFlags, open, openat};
use zstd::stream::write::Encoder;

use crate::bounded_io::BoundedIo;
use crate::{Limits, MAGIC, MAX_ENTRIES, VERSION, check_cancel, check_path, invalid};

/// Encodes a quiescent directory with sorted paths and normalized metadata.
///
/// This is not a filesystem snapshot. Links and special nodes are rejected.
/// Repeatability is within the same encoder version, not across zstd upgrades.
pub fn pack(path: &Path, limits: Limits, cancelled: impl Fn() -> bool) -> io::Result<Vec<u8>> {
    // Trailing `/` or `/.` otherwise makes the symlink an ancestor lookup that
    // NOFOLLOW does not protect. Components removes those spellings without
    // resolving symlinks or collapsing `..`.
    let path: std::path::PathBuf = path.components().collect();
    let root = File::from(open(
        &path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    let mut output = BoundedIo {
        inner: Vec::new(),
        remaining: limits.stored_bytes,
        cancelled: &cancelled,
    };
    output.write_all(MAGIC)?;
    output.write_all(&[VERSION])?;
    let mut encoder = Encoder::new(output, 3)?;
    encoder.include_checksum(true)?;
    encoder.window_log(23)?;
    let stream = BoundedIo {
        inner: encoder,
        remaining: limits.stream_bytes(),
        cancelled: &cancelled,
    };
    let mut builder = tar::Builder::new(stream);
    let mut entries = 0;
    let mut remaining = limits.expanded_bytes;
    append_directory(
        &root,
        "",
        &mut builder,
        &mut entries,
        &mut remaining,
        &cancelled,
    )?;
    builder.finish()?;
    let stream = builder.into_inner()?;
    Ok(stream.inner.finish()?.inner)
}

fn append_directory<W: Write>(
    directory: &File,
    prefix: &str,
    builder: &mut tar::Builder<W>,
    entries: &mut usize,
    remaining: &mut u64,
    cancelled: &impl Fn() -> bool,
) -> io::Result<()> {
    let mut names = Vec::new();
    for entry in Dir::read_from(directory)? {
        check_cancel(cancelled)?;
        let entry = entry?;
        let name = entry
            .file_name()
            .to_str()
            .map_err(|_| invalid("non-UTF-8 member name"))?;
        if name == "." || name == ".." {
            continue;
        }
        *entries += 1;
        if *entries > MAX_ENTRIES {
            return Err(invalid("too many directory members"));
        }
        names.push(name.to_owned());
    }
    names.sort();
    for name in names {
        check_cancel(cancelled)?;
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        check_path(&path)?;
        let mut file = File::from(openat(
            directory,
            name.as_str(),
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        let metadata = file.metadata()?;
        let mut header = tar::Header::new_ustar();
        header.set_path(&path)?;
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        if metadata.is_dir() {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_mode(0o700);
            header.set_size(0);
            header.set_cksum();
            builder.append(&header, io::empty())?;
            append_directory(&file, &path, builder, entries, remaining, cancelled)?;
        } else if metadata.is_file() {
            *remaining = remaining
                .checked_sub(metadata.len())
                .ok_or_else(|| invalid("expanded directory exceeds byte limit"))?;
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(0o600);
            header.set_size(metadata.len());
            header.set_cksum();
            let mut reader = BoundedIo {
                inner: Read::by_ref(&mut file).take(metadata.len()),
                remaining: metadata.len(),
                cancelled,
            };
            builder.append(&header, &mut reader)?;
            if reader.remaining != 0 || file.read(&mut [0u8; 1])? != 0 {
                return Err(invalid("directory source changed size during export"));
            }
        } else {
            return Err(invalid("directory contains a link or special file"));
        }
    }
    Ok(())
}
