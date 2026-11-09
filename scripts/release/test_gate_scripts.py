"""Tests for the release gate scripts assemble.sh and require_ci_green.sh."""

import json
import os
import shutil
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
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.tmp = Path(tmp.name)
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


def actions_run(conclusion, status="completed", app="github-actions"):
    return {"app": {"slug": app}, "conclusion": conclusion, "status": status}


@unittest.skipUnless(shutil.which("jq"), "jq is needed to apply require_ci_green.sh's --jq filter")
class RequireCiGreenTest(unittest.TestCase):
    def check(self, runs):
        """Run require_ci_green.sh against a fake `gh` that returns `runs` as the
        check-runs response and applies the script's own --jq filter to it."""
        with tempfile.TemporaryDirectory() as bin_dir:
            gh = Path(bin_dir) / "gh"
            gh.write_text(
                "#!/bin/sh\n"
                'while [ $# -gt 0 ]; do [ "$1" = --jq ] && filter="$2"; shift; done\n'
                'printf "%s" "$FAKE_RUNS" | jq -r "$filter"\n'
            )
            gh.chmod(0o755)
            env = {**os.environ, "PATH": f"{bin_dir}{os.pathsep}{os.environ['PATH']}",
                   "FAKE_RUNS": json.dumps({"check_runs": runs})}
            result = subprocess.run(["bash", HERE / "require_ci_green.sh", "o/r", "abc"],
                                    capture_output=True, text=True, env=env)
            return result.returncode, result.stdout

    def test_all_success_passes(self):
        self.assertEqual(self.check([actions_run("success")])[0], 0)
        self.assertEqual(self.check([actions_run("success"), actions_run("success")])[0], 0)

    def test_anything_else_fails(self):
        cases = {
            "success + cancelled": [actions_run("success"), actions_run("cancelled")],
            "no runs": [],
            "running": [actions_run(None, "in_progress")],
            "failure": [actions_run("failure")],
            "other app's success beside a running Actions run":
                [actions_run("success", app="evil"), actions_run(None, "in_progress")],
        }
        for name, runs in cases.items():
            with self.subTest(name):
                self.assertNotEqual(self.check(runs)[0], 0)

    def test_only_another_apps_success_counts_as_not_run(self):
        code, out = self.check([actions_run("success", app="evil")])
        self.assertNotEqual(code, 0)
        self.assertIn("has not run", out)


if __name__ == "__main__":
    unittest.main()
