//! Build-script support for package-owned, fresh source identity.
//!
//! Enabled by the `build` feature for use as a build-dependency. Nothing in
//! this module runs during extension startup or `self_info`.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(test)]
mod tests;

/// Captured owned repository identity and the inputs that can change it.
struct RepositoryMetadata {
    /// Full commit hash of the explicitly selected source checkout.
    revision: Option<String>,
    /// Whether tracked worktree or staged files differ from that commit.
    dirty: Option<bool>,
    /// Existing Git state and tracked paths that Cargo must observe.
    watches: BTreeSet<PathBuf>,
}

/// Capture identity only if the package directory is itself a Git checkout
/// root.
///
/// Call once from `build.rs`. Archives and packages nested beneath unrelated
/// repositories emit unknown metadata rather than borrowing an ancestor hash.
/// Missing or unreadable Git metadata also remains unknown. Package stamps
/// still take precedence over this fallback at runtime.
///
/// # Errors
///
/// Returns an error if Cargo did not provide the package manifest directory.
pub fn emit() -> io::Result<()> {
    emit_from_root(".")
}

/// Capture identity from an explicitly owned monorepo checkout root.
///
/// Relative roots resolve from `CARGO_MANIFEST_DIR`. Use this only when the
/// package's source belongs to that repository, e.g. `"../.."` for a workspace
/// crate under `crates/`. The root must have its own Git metadata, its index
/// must track the package manifest, and the package must not carry Cargo
/// archive or vendor markers. The helper never discovers an ancestor
/// repository.
///
/// Dirty state covers tracked changes, matching Git's `describe --dirty`
/// semantics. Cargo watches Git HEAD/index/refs and every tracked path,
/// including files outside Cargo's package include list. Missing tracked
/// inputs, gitlinks, file symlinks, and other unwatchable state yield unknown
/// identity while retaining valid Git/file watches. Restoring an input manually
/// can leave identity unknown until another watched Git/index/file change.
///
/// Owned source-directory symlinks use recursive watches; changes beneath them
/// can cause extra rebuilds. Targets outside the owned root, containing the
/// current build output, or overlapping Git metadata are not watched. Unchanged
/// builds do not rerun solely for metadata, including unknown states.
///
/// # Errors
///
/// Returns an error if Cargo did not provide the package manifest directory.
pub fn emit_from_root(root: impl AsRef<Path>) -> io::Result<()> {
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("Cargo manifest directory is unavailable"))?;
    let root = manifest.join(root);
    let metadata = RepositoryMetadata::capture(&manifest, &root);
    println!(
        "cargo:rerun-if-changed={}",
        manifest.join("Cargo.toml").display()
    );
    if let Some(metadata) = metadata {
        for path in metadata.watches {
            println!("cargo:rerun-if-changed={}", path.display());
        }
        println!(
            "cargo:rustc-env=TAU_PACKAGE_GIT_REVISION={}",
            metadata.revision.as_deref().unwrap_or("")
        );
        println!(
            "cargo:rustc-env=TAU_PACKAGE_GIT_DIRTY={}",
            metadata
                .dirty
                .map(|dirty| dirty.to_string())
                .unwrap_or_default()
        );
    } else {
        println!("cargo:rustc-env=TAU_PACKAGE_GIT_REVISION=");
        println!("cargo:rustc-env=TAU_PACKAGE_GIT_DIRTY=");
    }
    Ok(())
}

impl RepositoryMetadata {
    /// Fail closed to unknown when source ownership or refresh tracking is
    /// unclear.
    fn capture(manifest: &Path, root: &Path) -> Option<Self> {
        if manifest.join(".cargo_vcs_info.json").exists()
            || manifest.join(".cargo-checksum.json").exists()
        {
            return None;
        }
        let manifest = manifest.canonicalize().ok()?;
        let root = root.canonicalize().ok()?;
        let relative_manifest = manifest.strip_prefix(&root).ok()?.join("Cargo.toml");
        let repo = git2::Repository::open_ext(
            &root,
            git2::RepositoryOpenFlags::NO_SEARCH,
            std::iter::empty::<&str>(),
        )
        .ok()?;
        if repo.workdir()?.canonicalize().ok()? != root {
            return None;
        }
        let mut watches = BTreeSet::new();
        let dot_git = root.join(".git");
        if dot_git.is_file() {
            watches.insert(dot_git);
        }
        for directory in [repo.path(), repo.commondir()] {
            for name in [
                "HEAD",
                "index",
                "packed-refs",
                "refs",
                "config",
                "commondir",
            ] {
                let path = directory.join(name);
                if path.exists() {
                    watches.insert(path);
                }
            }
        }
        // Cargo's line-oriented directives cannot faithfully watch these paths.
        if watches
            .iter()
            .any(|path| path.to_str().is_none_or(|path| path.contains(['\n', '\r'])))
        {
            return None;
        }
        let mut metadata = Self {
            revision: None,
            dirty: None,
            watches,
        };
        // An owned unborn repository still needs Git watches so its first
        // commit can make identity available without touching the
        // package's sources.
        let Ok(index) = repo.index() else {
            return Some(metadata);
        };
        if index.get_path(&relative_manifest, 0).is_none() {
            return Some(metadata);
        }
        metadata.revision = repo
            .head()
            .and_then(|head| head.peel_to_commit())
            .ok()
            .map(|commit| commit.id().to_string());
        let out_dir =
            std::env::var_os("OUT_DIR").and_then(|path| PathBuf::from(path).canonicalize().ok());
        let git_dirs = [repo.path(), repo.commondir()];
        let mut complete = true;
        for entry in index.iter() {
            let Ok(relative) = std::str::from_utf8(&entry.path) else {
                complete = false;
                continue;
            };
            let path = root.join(relative);
            if relative.contains(['\n', '\r']) || entry.mode == 0o160000 {
                complete = false;
                continue;
            }
            let Ok(info) = path.symlink_metadata() else {
                // Watching a missing path makes every Cargo invocation stale.
                complete = false;
                continue;
            };
            if info.is_symlink() {
                let Ok(target) = path.canonicalize() else {
                    complete = false;
                    continue;
                };
                if !target.starts_with(&root)
                    || out_dir.as_ref().is_some_and(|out| out.starts_with(&target))
                    || git_dirs.iter().any(|directory| {
                        directory.starts_with(&target) || target.starts_with(directory)
                    })
                {
                    complete = false;
                    continue;
                }
                if !target.is_dir() {
                    // Cargo observes a file symlink's target mtime, not the
                    // link mtime; retargeting it can
                    // otherwise leave a false clean flag.
                    complete = false;
                }
            } else if !info.is_file() {
                complete = false;
                continue;
            }
            metadata.watches.insert(path);
        }
        if complete {
            let mut options = git2::StatusOptions::new();
            options.include_untracked(false).include_ignored(false);
            metadata.dirty = repo.statuses(Some(&mut options)).ok().map(|statuses| {
                statuses
                    .iter()
                    .any(|status| status.status() != git2::Status::CURRENT)
            });
        }
        if metadata.dirty.is_none() {
            // A known commit alone must not imply fresh clean/dirty state.
            metadata.revision = None;
        }
        Some(metadata)
    }
}
