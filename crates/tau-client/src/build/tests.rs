use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use super::RepositoryMetadata;

/// Isolated repository whose identity never depends on the developer checkout.
struct Fixture {
    /// Owns and removes all fixture files.
    _directory: tempfile::TempDir,
    /// Canonical source root, matching the paths recorded by capture.
    root: PathBuf,
    /// Repository manipulated through libgit2 without hooks or user identity.
    repo: git2::Repository,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("create fixture directory");
        let root = directory
            .path()
            .canonicalize()
            .expect("canonical fixture root");
        let repo = git2::Repository::init(&root).expect("initialize repository");
        fs::write(root.join("Cargo.toml"), "[package]\n").expect("write manifest");
        Self {
            _directory: directory,
            root,
            repo,
        }
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn commit(&self, paths: &[&str]) -> String {
        let mut index = self.repo.index().expect("open index");
        for path in paths {
            index.add_path(Path::new(path)).expect("stage fixture path");
        }
        index.write().expect("write index");
        let tree_id = index.write_tree().expect("write tree");
        let tree = self.repo.find_tree(tree_id).expect("find tree");
        let signature =
            git2::Signature::now("Build test", "test@example.invalid").expect("create signature");
        let parent = self
            .repo
            .head()
            .ok()
            .and_then(|head| head.peel_to_commit().ok());
        let parents: Vec<_> = parent.iter().collect();
        self.repo
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "fixture",
                &tree,
                &parents,
            )
            .expect("commit fixture")
            .to_string()
    }

    fn capture(&self) -> RepositoryMetadata {
        RepositoryMetadata::capture(self.root(), self.root()).expect("owned repository")
    }
}

/// Tracked edits, staged edits and HEAD changes must update identity, while
/// untracked files must not mark an otherwise clean package dirty.
#[test]
fn tracked_identity_and_refresh_inputs_follow_repository_state() {
    let fixture = Fixture::new();
    fs::write(fixture.root().join("source"), "original").expect("write source");
    let revision = fixture.commit(&["Cargo.toml", "source"]);
    let clean = fixture.capture();
    assert_eq!(clean.revision.as_deref(), Some(revision.as_str()));
    assert_eq!(clean.dirty, Some(false));
    for path in [
        fixture.root().join("Cargo.toml"),
        fixture.root().join("source"),
        fixture.repo.path().join("HEAD"),
        fixture.repo.path().join("index"),
        fixture.repo.path().join("refs"),
    ] {
        assert!(clean.watches.contains(&path), "missing watch: {path:?}");
    }

    fs::write(fixture.root().join("untracked"), "ignored by identity").expect("write untracked");
    assert_eq!(fixture.capture().dirty, Some(false));
    fs::write(fixture.root().join("source"), "worktree edit").expect("edit source");
    assert_eq!(fixture.capture().dirty, Some(true));
    let mut index = fixture.repo.index().expect("open index");
    index.add_path(Path::new("source")).expect("stage edit");
    index.write().expect("write index");
    assert_eq!(fixture.capture().dirty, Some(true));
    fs::write(fixture.root().join("source"), "original").expect("restore worktree");
    assert_eq!(fixture.capture().dirty, Some(true));
    fs::write(fixture.root().join("source"), "committed edit").expect("edit source");
    let next = fixture.commit(&["source"]);
    assert_ne!(revision, next);
    let clean = fixture.capture();
    assert_eq!(clean.revision.as_deref(), Some(next.as_str()));
    assert_eq!(clean.dirty, Some(false));
}

/// Explicit ownership must reject archive/vendor markers, ancestor discovery
/// and manifests not tracked by the selected repository.
#[test]
fn package_ownership_cannot_borrow_unrelated_identity() {
    let fixture = Fixture::new();
    fixture.commit(&["Cargo.toml"]);
    for marker in [".cargo_vcs_info.json", ".cargo-checksum.json"] {
        let path = fixture.root().join(marker);
        fs::write(&path, "{}").expect("write marker");
        assert!(RepositoryMetadata::capture(fixture.root(), fixture.root()).is_none());
        fs::remove_file(path).expect("remove marker");
    }
    let nested = fixture.root().join("nested");
    fs::create_dir(&nested).expect("create nested package");
    fs::write(nested.join("Cargo.toml"), "[package]\n").expect("write nested manifest");
    assert!(RepositoryMetadata::capture(&nested, &nested).is_none());
    let untracked =
        RepositoryMetadata::capture(&nested, fixture.root()).expect("owned root without manifest");
    assert!(untracked.revision.is_none());
    assert!(untracked.dirty.is_none());
    fixture.commit(&["nested/Cargo.toml"]);
    assert_eq!(
        RepositoryMetadata::capture(&nested, fixture.root())
            .expect("tracked workspace package")
            .dirty,
        Some(false)
    );
    let outside = tempfile::tempdir().expect("create unrelated directory");
    assert!(RepositoryMetadata::capture(outside.path(), fixture.root()).is_none());
}

/// Unborn repositories must stay unknown but watch Git state so a first commit
/// can refresh metadata without a source edit.
#[test]
fn unborn_repository_retains_git_refresh_watches() {
    let fixture = Fixture::new();
    let metadata = fixture.capture();
    assert!(metadata.revision.is_none());
    assert!(metadata.dirty.is_none());
    assert!(metadata.watches.contains(&fixture.repo.path().join("HEAD")));
    assert_eq!(fixture.capture().revision, None);
    let revision = fixture.commit(&["Cargo.toml"]);
    assert_eq!(
        fixture.capture().revision.as_deref(),
        Some(revision.as_str())
    );
}

/// Missing tracked files and paths Cargo cannot represent must not claim a
/// fresh revision, but valid files must retain their refresh watches.
#[test]
fn unwatchable_tracked_inputs_fail_closed_without_losing_valid_watches() {
    for name in ["missing", "line\nbreak", "carriage\rreturn"] {
        let fixture = Fixture::new();
        fs::write(fixture.root().join(name), "tracked").expect("write tracked input");
        fixture.commit(&["Cargo.toml", name]);
        if name == "missing" {
            fs::remove_file(fixture.root().join(name)).expect("remove tracked input");
        }
        let metadata = fixture.capture();
        assert!(
            metadata.revision.is_none(),
            "unexpected identity for {name:?}"
        );
        assert!(metadata.dirty.is_none());
        assert!(
            metadata
                .watches
                .contains(&fixture.root().join("Cargo.toml"))
        );
        assert!(!metadata.watches.contains(&fixture.root().join(name)));
    }
}

/// File symlinks cannot provide freshness; owned directory targets can be
/// watched, while outside and Git-metadata targets must never be watched.
#[test]
fn symlink_watchability_controls_identity_and_watch_scope() {
    let fixture = Fixture::new();
    fs::write(fixture.root().join("source"), "source").expect("write source");
    let link = fixture.root().join("link");
    symlink("source", &link).expect("create file symlink");
    fixture.commit(&["Cargo.toml", "source", "link"]);
    let file = fixture.capture();
    assert!(file.revision.is_none());
    assert!(file.watches.contains(&link));

    fs::create_dir(fixture.root().join("directory")).expect("create directory target");
    fs::remove_file(&link).expect("remove file symlink");
    symlink("directory", &link).expect("create directory symlink");
    assert_eq!(fixture.capture().dirty, Some(true));
    assert!(fixture.capture().watches.contains(&link));

    let outside = tempfile::tempdir().expect("create outside target");
    for target in [outside.path(), fixture.repo.path(), Path::new("absent")] {
        fs::remove_file(&link).expect("remove previous symlink");
        symlink(target, &link).expect("create unsafe symlink");
        let metadata = fixture.capture();
        assert!(metadata.revision.is_none());
        assert!(metadata.dirty.is_none());
        assert!(!metadata.watches.contains(&link));
    }
}
