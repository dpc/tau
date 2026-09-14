#!/usr/bin/env python3
"""Publisher state-machine tests. Never contact GitHub or mutate a real release."""

import copy
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import native
import publish


class PublisherTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.names = ["first.deb", "second.rpm", "SHA256SUMS"]
        for name in self.names:
            (self.root / name).write_text(name)
        self.sha = "a" * 40
        self.release = None
        self.assets = []
        self.calls = []
        self.fail_upload = None
        self.fail_finalize = False

    def api(self, endpoint):
        if "/assets?" in endpoint:
            return copy.deepcopy(self.assets)
        return [copy.deepcopy(self.release)] if self.release else []

    def gh(self, *args):
        self.calls.append(args)
        operation = args[1]
        if operation == "create":
            self.release = {
                "id": 12, "tag_name": "v0.1.1", "target_commitish": self.sha,
                "draft": True, "prerelease": False,
                "body": args[args.index("--notes") + 1],
            }
        elif operation == "upload":
            path = Path(args[3])
            if path.name == self.fail_upload:
                raise OSError("interrupted upload")
            self.assets.append({
                "name": path.name, "state": "uploaded", "size": path.stat().st_size,
                "digest": f"sha256:{native.sha256(path.read_bytes())}",
            })
        elif operation == "edit":
            if self.fail_finalize:
                raise OSError("failed finalization")
            self.release["draft"] = False
        else:
            self.fail(f"unexpected mutation {args}")
        return ""

    def execute(self, workflow=None):
        with patch.object(publish, "gh", side_effect=self.gh), \
                patch.object(publish, "api_pages", side_effect=self.api), \
                patch.object(publish, "verify_tag"):
            return publish.publish("dpc/tau", "v0.1.1", self.sha, workflow or self.sha,
                                   self.root, self.names)

    def test_interrupted_upload_resumes_only_missing_and_completed_retry_is_read_only(self):
        self.fail_upload = "second.rpm"
        with self.assertRaisesRegex(OSError, "interrupted"):
            self.execute()
        self.assertTrue(self.release["draft"])
        existing = {a["name"] for a in self.assets}
        self.assertTrue(existing)
        self.calls.clear()
        self.fail_upload = None
        self.assertEqual(self.execute(), "published-verified")
        uploads = [Path(c[3]).name for c in self.calls if c[1] == "upload"]
        self.assertEqual(set(uploads), set(self.names) - existing)
        self.assertFalse(any("--clobber" in c for c in self.calls))
        self.calls.clear()
        self.assertEqual(self.execute(), "already-published-verified")
        self.assertEqual(self.calls, [])

    def test_foreign_draft_and_conflicting_assets_never_mutate(self):
        self.fail_upload = "second.rpm"
        with self.assertRaises(OSError):
            self.execute()
        body = self.release["body"]
        self.release["body"] = "foreign draft"
        self.calls.clear()
        with self.assertRaisesRegex(ValueError, "foreign"):
            self.execute()
        self.assertEqual(self.calls, [])
        self.release["body"] = body
        self.assets[0]["digest"] = "sha256:" + "b" * 64
        with self.assertRaisesRegex(ValueError, "conflicting"):
            self.execute()
        self.assertEqual(self.calls, [])

    def test_failed_finalization_resumes_without_reupload(self):
        self.fail_finalize = True
        with self.assertRaisesRegex(OSError, "finalization"):
            self.execute()
        self.assertTrue(self.release["draft"])
        self.assertEqual(len(self.assets), len(self.names))
        self.calls.clear()
        self.fail_finalize = False
        self.assertEqual(self.execute(), "published-verified")
        self.assertEqual([c[1] for c in self.calls], ["edit"])

    def test_source_tooling_mismatch_and_published_incomplete_release_fail_closed(self):
        with self.assertRaisesRegex(ValueError, "identical"):
            self.execute("c" * 40)
        self.assertEqual(self.calls, [])
        self.execute()
        self.assets.pop()
        self.calls.clear()
        with self.assertRaisesRegex(ValueError, "published release is incomplete"):
            self.execute()
        self.assertEqual(self.calls, [])

    def test_remote_tag_must_resolve_to_exact_commit(self):
        for refs in ("", f"{'b' * 40}\trefs/tags/v0.1.1\n"):
            with patch.object(native, "run", return_value=refs), self.assertRaises(ValueError):
                publish.verify_tag("dpc/tau", "v0.1.1", self.sha)
        with patch.object(native, "run", return_value=(
            f"{'b' * 40}\trefs/tags/v0.1.1\n{self.sha}\trefs/tags/v0.1.1^{{}}\n"
        )):
            publish.verify_tag("dpc/tau", "v0.1.1", self.sha)


if __name__ == "__main__":
    unittest.main()
