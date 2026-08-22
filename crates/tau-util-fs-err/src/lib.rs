//! Path-aware filesystem operations used at reviewed Tau error boundaries.
//!
//! This deliberately thin façade fixes Tau's `fs-err` version and uses only
//! its default features: `debug`, `debug_tokio`, and `expose_original_error`
//! stay disabled. Callers retain ordinary [`std::io::Result`] values.
//! Contextual errors preserve [`std::io::ErrorKind`] and display the underlying
//! OS reason, but their outer [`std::io::Error::raw_os_error`] is `None` and
//! [`std::error::Error::source`] returns `None`.
//!
//! Error display includes the complete supplied path through lossy, unescaped
//! [`std::path::Path::display`]. Invalid bytes become replacement characters,
//! control characters remain active, and no sensitive-path redaction occurs.
//! Raw-errno branches, typed domain errors, and redacted, model-visible,
//! provider-visible, or secret-facing boundaries must therefore keep their
//! purpose-built implementations. Public Tau APIs should expose
//! [`std::io::Error`] rather than façade wrapper types. Call
//! [`OpenOptions::open`] directly because `OpenOptions::options().open(...)`
//! bypasses path context.

pub use fs_err::{
    DirEntry, File, OpenOptions, PathExt, ReadDir, canonicalize, copy, create_dir, create_dir_all,
    hard_link, metadata, read, read_dir, read_link, read_to_string, remove_dir, remove_dir_all,
    remove_file, rename, set_permissions, symlink_metadata, write,
};

#[cfg(test)]
mod tests;
