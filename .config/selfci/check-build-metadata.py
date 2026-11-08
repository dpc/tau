#!/usr/bin/env python3
"""Exercise shared build metadata with real Cargo caching and isolated Git repos."""

import os
import pathlib
import shutil
import subprocess
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[2]
ENV = dict(os.environ)
for key in ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR"):
    ENV.pop(key, None)
ENV.update(
    GIT_CONFIG_NOSYSTEM="1",
    GIT_CONFIG_GLOBAL=os.devnull,
    TAU_BUILD="inherited-core",
    TAU_PACKAGE_GIT_REVISION="a" * 40,
    TAU_PACKAGE_GIT_DIRTY="true",
)


def run(directory, *command):
    """Run a fixture command, retaining diagnostics only on failure."""
    result = subprocess.run(
        command, cwd=directory, env=ENV, text=True, capture_output=True, check=False
    )
    if result.returncode:
        raise RuntimeError(f"{command!r} failed:\n{result.stdout}\n{result.stderr}")
    return result.stdout.strip()


def git(directory, *arguments):
    """Modify only fixture repositories, ignoring user hooks and signing."""
    return run(
        directory, "git", "-c", "commit.gpgsign=false",
        "-c", "core.hooksPath=" + os.devnull, *arguments,
    )


def initialize(directory):
    """Create an owned repository with a non-secret local commit identity."""
    directory.mkdir(parents=True, exist_ok=True)
    git(directory, "init", "-q")
    git(directory, "config", "user.email", "test@example.invalid")
    git(directory, "config", "user.name", "Build metadata test")


def fixture(directory, root=None):
    """Use the real SDK helper and macro in a tiny independently versioned package."""
    directory.mkdir(parents=True, exist_ok=True)
    client = str(ROOT / "crates/tau-client")
    (directory / "Cargo.toml").write_text(
        '[package]\nname="tau-build-metadata-probe"\nversion="9.8.7"\n'
        'edition="2024"\ninclude=["build.rs", "src/**"]\n'
        '[workspace]\nresolver="2"\n'
        '[profile.dev]\ndebug=false\n'
        f'[dependencies]\ntau-client={{package="dpc-tau-client",path="{client}"}}\n'
        f'[build-dependencies]\ntau-client={{package="dpc-tau-client",path="{client}",features=["build"]}}\n'
    )
    call = "emit()" if root is None else f'emit_from_root("{root}")'
    (directory / "build.rs").write_text(
        'fn main() {\n'
        f'    tau_client::build::{call}.expect("capture identity");\n'
        '    let counter = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("counter");\n'
        '    let count = std::fs::read_to_string(&counter).ok().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0) + 1;\n'
        '    std::fs::write(counter, count.to_string()).unwrap();\n'
        '    println!("cargo:rustc-env=PROBE_BUILD_COUNT={count}");\n'
        '}\n'
    )
    (directory / "src").mkdir()
    (directory / "src/main.rs").write_text(
        'fn main() {\n'
        '    let identity = tau_client::package_build_identity!();\n'
        '    println!("{}|{}|{}", identity.release, identity.revision.as_deref().unwrap_or("unknown"), env!("PROBE_BUILD_COUNT"));\n'
        '}\n'
    )
    (directory / ".gitignore").write_text("/target\n")
    (directory / "excluded.txt").write_text("tracked but excluded from Cargo packaging\n")
    shutil.copyfile(ROOT / "Cargo.lock", directory / "Cargo.lock")
    # Explicitly populate the required archive cache, including in fresh CI
    # Cargo homes. Subsequent scenario builds are locked and offline.
    run(directory, "cargo", "fetch", "--quiet")


def check(directory, revision, dirty=False):
    """Assert the compiled identity and return a build-script invocation counter."""
    output = run(directory, "cargo", "run", "--locked", "--offline", "--quiet")
    release, observed, count = output.split("|")
    expected = "unknown" if revision is None else revision[:7] + ("-modified" if dirty else "")
    assert (release, observed) == ("9.8.7", expected), (directory, output, expected)
    return int(count)


with tempfile.TemporaryDirectory(prefix="tau-build-metadata-") as temporary:
    base = pathlib.Path(temporary)
    # Keep CARGO_TARGET_DIR unchanged across every scenario. The later output-link
    # check needs build artifacts inside the unborn repo, so start there even
    # before that repo is initialized. Switching to a fresh target for that check
    # would compile the entire dependency graph twice.
    # SelfCI checkouts are disposable; keep these shared artifacts fixture-owned.
    ENV["CARGO_TARGET_DIR"] = str(base / "unborn" / "target")
    own = base / "own"
    fixture(own)
    initialize(own)
    git(own, "add", ".")
    git(own, "commit", "-qm", "initial")
    revision = git(own, "rev-parse", "HEAD")
    count = check(own, revision)
    assert check(own, revision) == count, "unchanged builds must not rerun"
    (own / "excluded.txt").write_text("modified outside Cargo include list\n")
    check(own, revision, True)
    git(own, "add", "excluded.txt")
    git(own, "commit", "-qm", "commit without changing Cargo build inputs")
    check(own, git(own, "rev-parse", "HEAD"))
    git(own, "commit", "--amend", "-qm", "amended commit without source edits")
    check(own, git(own, "rev-parse", "HEAD"))
    git(own, "pack-refs", "--all", "--prune")
    check(own, git(own, "rev-parse", "HEAD"))
    git(own, "commit", "--allow-empty", "-qm", "move packed ref")
    check(own, git(own, "rev-parse", "HEAD"))
    (own / ".git" / "packed-refs").unlink()
    count = check(own, git(own, "rev-parse", "HEAD"))
    assert check(own, git(own, "rev-parse", "HEAD")) == count, "deleted optional refs must not force reruns"
    (own / "excluded.txt").unlink()
    count = check(own, None)
    assert check(own, None) == count, "missing tracked paths must not force reruns"
    git(own, "restore", "excluded.txt")
    git(own, "add", "excluded.txt")
    check(own, git(own, "rev-parse", "HEAD"))

    worktree = base / "linked"
    git(own, "worktree", "add", "--detach", str(worktree), "HEAD")
    check(worktree, git(worktree, "rev-parse", "HEAD"))
    git(worktree, "commit", "--allow-empty", "-qm", "move linked HEAD")
    check(worktree, git(worktree, "rev-parse", "HEAD"))
    (worktree / "excluded.txt").write_text("linked worktree dirty\n")
    check(worktree, git(worktree, "rev-parse", "HEAD"), True)

    enclosing = base / "consumer"
    initialize(enclosing)
    (enclosing / "consumer.txt").write_text("unrelated repository\n")
    git(enclosing, "add", ".")
    git(enclosing, "commit", "-qm", "consumer")
    archive = enclosing / "archive"
    fixture(archive)
    count = check(archive, None)
    assert check(archive, None) == count, "archives must not watch a missing .git"

    unborn = base / "unborn"
    fixture(unborn)
    initialize(unborn)
    git(unborn, "add", ".")
    count = check(unborn, None)
    assert check(unborn, None) == count, "unborn HEAD must not force reruns"
    git(unborn, "commit", "-qm", "first commit without source edits")
    check(unborn, git(unborn, "rev-parse", "HEAD"))

    # A directory gitlink is not a request to scan a child repository's target.
    module = base / "module-source"
    initialize(module)
    (module / "module.txt").write_text("submodule source\n")
    git(module, "add", ".")
    git(module, "commit", "-qm", "module")
    git(own, "-c", "protocol.file.allow=always", "submodule", "add", "-q", str(module), "module")
    git(own, "commit", "-qm", "add gitlink")
    count = check(own, None)
    (own / "module" / "target").mkdir()
    (own / "module" / "target" / "output").write_text("untracked module build output\n")
    assert check(own, None) == count, "gitlinks must not recursively watch module artifacts"

    monorepo = base / "monorepo"
    package = monorepo / "crates" / "extension"
    fixture(package, "../..")
    initialize(monorepo)
    git(monorepo, "add", ".")
    git(monorepo, "commit", "-qm", "owned monorepo")
    check(package, git(monorepo, "rev-parse", "HEAD"))

    # Match Tau's owned source-directory symlink and root file-symlink layout.
    (monorepo / ".agents" / "skills").mkdir(parents=True)
    (monorepo / ".agents" / "skills" / "example.md").write_text("source skill\n")
    (monorepo / ".agents" / "other").mkdir()
    (monorepo / ".agents" / "other" / "example.md").write_text("older alternative\n")
    (monorepo / ".claude").mkdir()
    (monorepo / ".claude" / "skills").symlink_to("../.agents/skills", target_is_directory=True)
    git(monorepo, "add", ".")
    git(monorepo, "commit", "-qm", "owned directory symlink")
    revision = git(monorepo, "rev-parse", "HEAD")
    count = check(package, revision)
    assert check(package, revision) == count, "owned source symlinks must preserve no-op caching"
    (monorepo / ".claude" / "skills").unlink()
    (monorepo / ".claude" / "skills").symlink_to("../.agents/other", target_is_directory=True)
    check(package, revision, True)
    (monorepo / "AGENTS.md").write_text("source instructions\n")
    (monorepo / "CLAUDE.md").symlink_to("AGENTS.md")
    git(monorepo, "add", ".")
    git(monorepo, "commit", "-qm", "file symlink has unwatchable link mtime")
    count = check(package, None)
    assert check(package, None) == count, "file-symlink uncertainty must not force reruns"

    (unborn / "git-link").symlink_to(".git", target_is_directory=True)
    git(unborn, "add", "git-link")
    git(unborn, "commit", "-qm", "unsupported Git metadata symlink")
    count = check(unborn, None)
    object_source = base / "object.txt"
    object_source.write_text("unreachable object\n")
    git(unborn, "hash-object", "-w", str(object_source))
    assert check(unborn, None) == count, "Git object writes must not be watched recursively"

    # Reuse the target chosen before the first build; do not switch directories
    # here just to put the symlink's destination inside this repository.
    (unborn / "output-link").symlink_to("target", target_is_directory=True)
    git(unborn, "add", "output-link")
    git(unborn, "commit", "-qm", "unsupported build-output symlink")
    count = check(unborn, None)
    (unborn / "target" / "unrelated-output").write_text("build output\n")
    assert check(unborn, None) == count, "build output directories must not be watched"
    for marker in (".cargo_vcs_info.json", ".cargo-checksum.json"):
        marked = enclosing / marker.removeprefix(".")
        fixture(marked, "..")
        (marked / marker).write_text("{}")
        git(enclosing, "add", str(marked))
        git(enclosing, "commit", "-qm", "tracked package archive")
        check(marked, None)

print("build metadata: ownership, archive isolation, Git freshness, linked worktree, and unchanged-build caching passed")
