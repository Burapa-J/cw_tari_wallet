"""Tests for the release gate scripts assemble.sh and require_ci_green.sh."""

import os
import subprocess
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
PLATFORMS = ["android", "ios", "macos", "linux-x86_64", "linux-aarch64", "windows"]
TAG = "v1.2.3"


def run(cmd, env=None):
    return subprocess.run(cmd, capture_output=True, text=True, env=env).returncode


class AssembleTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.artifacts = self.tmp / "artifacts"
        for platform in PLATFORMS:
            d = self.artifacts / f"release-{platform}"
            d.mkdir(parents=True)
            (d / f"cw_tari_wallet-{TAG}-{platform}.zip").write_text("zip")
            (d / f"rustc-{platform}.txt").write_text("rustc 1.0.0")

    def assemble(self):
        return run(["bash", HERE / "assemble.sh", TAG, self.artifacts,
                    self.tmp / "dist", self.tmp / "rustc", *PLATFORMS])

    def test_all_platforms_pass(self):
        self.assertEqual(self.assemble(), 0)
        self.assertEqual(len(list((self.tmp / "dist").iterdir())), len(PLATFORMS))

    def test_extra_file_fails(self):
        (self.artifacts / "release-ios" / "extra.txt").write_text("x")
        self.assertNotEqual(self.assemble(), 0)

    def test_other_platforms_zip_fails(self):
        (self.artifacts / "release-android" / f"cw_tari_wallet-{TAG}-ios.zip").write_text("evil")
        self.assertNotEqual(self.assemble(), 0)

    def test_missing_artifact_fails(self):
        for f in (self.artifacts / "release-windows").iterdir():
            f.unlink()
        (self.artifacts / "release-windows").rmdir()
        self.assertNotEqual(self.assemble(), 0)

    def test_extra_artifact_fails(self):
        (self.artifacts / "release-extra").mkdir()
        self.assertNotEqual(self.assemble(), 0)


class RequireCiGreenTest(unittest.TestCase):
    def check(self, states):
        """Run require_ci_green.sh with a fake `gh` that reports `states` for every check."""
        bin_dir = Path(tempfile.mkdtemp())
        gh = bin_dir / "gh"
        gh.write_text('#!/bin/sh\nprintf "%s" "$FAKE_STATES"\n')
        gh.chmod(0o755)
        env = {**os.environ, "PATH": f"{bin_dir}{os.pathsep}{os.environ['PATH']}",
               "FAKE_STATES": states}
        return run(["bash", HERE / "require_ci_green.sh", "o/r", "abc"], env)

    def test_all_success_passes(self):
        self.assertEqual(self.check("success\n"), 0)
        self.assertEqual(self.check("success\nsuccess\n"), 0)

    def test_anything_else_fails(self):
        for states in ["success\ncancelled\n", "", "in_progress\n", "failure\n"]:
            with self.subTest(states=states):
                self.assertNotEqual(self.check(states), 0)


if __name__ == "__main__":
    unittest.main()
