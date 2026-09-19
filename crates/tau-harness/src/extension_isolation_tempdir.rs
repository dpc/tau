//! Cleanup ownership for one supervised extension's isolation tree.

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::{fs, io};

/// Owned temporary tree used as bind-mount sources for one supervised child.
///
/// Provider snapshot materialization deliberately removes owner write access
/// while the tree is live. Disposal restores write access only on that known
/// harness-owned directory so ordinary recursive temporary-directory cleanup
/// can unlink its read-only files.
pub(crate) struct ExtensionIsolationTempDir {
    /// Underlying temporary directory removed after permission repair.
    tempdir: tempfile::TempDir,
}

impl ExtensionIsolationTempDir {
    /// Creates the cleanup owner before any isolation paths are materialized.
    pub(crate) fn new() -> io::Result<Self> {
        tempfile::Builder::new()
            .prefix("tau-extension-state-mask-")
            .tempdir()
            .map(|tempdir| Self { tempdir })
    }

    /// Returns the root used to build and retain child bind-mount sources.
    pub(crate) fn path(&self) -> &Path {
        self.tempdir.path()
    }

    /// Returns the one directory whose launch-time mode blocks later cleanup.
    fn provider_snapshot_path(&self) -> std::path::PathBuf {
        self.path().join("provider-profile-snapshot")
    }
}

impl Drop for ExtensionIsolationTempDir {
    fn drop(&mut self) {
        let snapshot = self.provider_snapshot_path();
        if fs::symlink_metadata(&snapshot).is_ok_and(|metadata| metadata.is_dir()) {
            let _ = fs::set_permissions(snapshot, fs::Permissions::from_mode(0o700));
        }
    }
}
