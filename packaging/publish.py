#!/usr/bin/env python3
"""Resume only matching Tau drafts; publish only a remotely verified complete set."""

import argparse
import json
from pathlib import Path
import subprocess

import native
import release_assets


def gh(*args):
    return subprocess.check_output(["gh", *args], text=True, timeout=300)


def api_pages(endpoint):
    pages = json.loads(gh("api", "--paginate", "--slurp", endpoint))
    return [item for page in pages for item in page]


def verify_tag(repo, tag, source_sha):
    refs = {}
    for line in native.run("git", "ls-remote", f"https://github.com/{repo}.git",
                           f"refs/tags/{tag}", f"refs/tags/{tag}^{{}}").splitlines():
        sha, name = line.split("\t")
        if name in refs:
            raise ValueError("duplicate remote tag ref")
        refs[name] = sha
    direct, peeled = f"refs/tags/{tag}", f"refs/tags/{tag}^{{}}"
    if not refs or not set(refs) <= {direct, peeled} or refs.get(peeled, refs.get(direct)) != source_sha:
        raise ValueError("remote tag no longer identifies the selected source")


def find_release(repo, tag):
    matches = [r for r in api_pages(f"repos/{repo}/releases?per_page=100")
               if r["tag_name"] == tag]
    if len(matches) > 1:
        raise ValueError("ambiguous release identity")
    return matches[0] if matches else None


def identity(release, tag, source_sha, marker):
    # target_commitish is only a creation hint when the tag already exists.
    # The exact remote tag plus our source/workflow marker bind the identity.
    if (release["tag_name"] != tag
            or marker not in (release["body"] or "")
            or release["prerelease"] != native.release_is_prerelease(tag[1:])):
        raise ValueError("refusing an unrelated release or foreign draft")


def existing_assets(repo, release, expected):
    actual = {}
    for asset in api_pages(f"repos/{repo}/releases/{release['id']}/assets?per_page=100"):
        name = asset["name"]
        if name in actual or name not in expected:
            raise ValueError("unexpected or duplicate remote asset")
        digest, size = expected[name]
        if (asset["state"] != "uploaded" or asset.get("digest") != f"sha256:{digest}"
                or asset["size"] != size):
            raise ValueError(f"conflicting remote asset: {name}; no overwrite or deletion")
        actual[name] = asset
    return actual


def publish(repo, tag, source_sha, workflow_sha, source, filenames):
    """Network mutation boundary; callers must first verify the local asset set."""
    native.require_sha(source_sha)
    if workflow_sha != source_sha:
        raise ValueError("publication requires identical source and workflow SHAs")
    if repo != "dpc/tau":
        raise ValueError("publisher is restricted to dpc/tau")
    marker = f"<!-- tau-native-release-v1 tag={tag} source={source_sha} workflow={workflow_sha} -->"
    expected = {name: (native.sha256((source / name).read_bytes()),
                       (source / name).stat().st_size) for name in filenames}
    verify_tag(repo, tag, source_sha)
    release = find_release(repo, tag)
    if release is None:
        gh("release", "create", tag, "--repo", repo, "--draft", "--verify-tag",
           "--target", source_sha, "--generate-notes", "--notes", marker,
           *(["--prerelease"] if native.release_is_prerelease(tag[1:]) else []))
        release = find_release(repo, tag)
        if release is None:
            raise ValueError("created draft is not visible; retry without changing assets")
    identity(release, tag, source_sha, marker)
    actual = existing_assets(repo, release, expected)
    if not release["draft"]:
        if set(actual) != set(expected):
            raise ValueError("published release is incomplete; refusing any mutation")
        return "already-published-verified"
    for name in sorted(set(expected) - set(actual)):
        # No --clobber: a race, partial starter asset, or differing upload fails
        # closed and leaves the draft for explicit owner investigation.
        gh("release", "upload", tag, str(source / name), "--repo", repo)
    release = find_release(repo, tag)
    if release is None:
        raise ValueError("draft disappeared")
    identity(release, tag, source_sha, marker)
    if set(existing_assets(repo, release, expected)) != set(expected):
        raise ValueError("remote asset inventory is incomplete")
    verify_tag(repo, tag, source_sha)
    if release["draft"]:
        gh("release", "edit", tag, "--repo", repo, "--draft=false")
    final = find_release(repo, tag)
    if final is None:
        raise ValueError("release disappeared during finalization")
    identity(final, tag, source_sha, marker)
    if final["draft"] or set(existing_assets(repo, final, expected)) != set(expected):
        raise ValueError("release finalization did not complete; safe to retry")
    return "published-verified"


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--source-repo", type=Path, required=True)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--source-sha", type=native.require_sha, required=True)
    parser.add_argument("--workflow-sha", type=native.require_sha, required=True)
    parser.add_argument("--tag", required=True)
    args = vars(parser.parse_args())
    source_repo = args.pop("source_repo")
    source = args["source"]
    filenames = release_assets.verify(source, source_repo, args["source_sha"], args["tag"])
    (source / "SHA256SUMS").write_text("".join(
        f"{native.sha256((source / name).read_bytes())}  {name}\n" for name in filenames
    ))
    print(publish(**args, filenames=[*filenames, "SHA256SUMS"]))
