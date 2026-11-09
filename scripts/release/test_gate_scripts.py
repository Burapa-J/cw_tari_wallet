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


RUNS_URL = "repos/o/r/actions/workflows/ci.yml/runs?head_sha=abc&event=push&branch=main"
CHECKS = ["bridge codegen drift", "forbidden crates", "cargo-deny"]


def jobs(conclusion, status="completed", names=CHECKS):
    return {"jobs": [{"name": n, "conclusion": conclusion, "status": status} for n in names]}


def api(ci_runs, jobs_by_run):
    """Fake GitHub API: ci.yml's push runs on main for `abc`, plus jobs per run id."""
    responses = {RUNS_URL: {"workflow_runs": ci_runs}}
    for run_id, body in jobs_by_run.items():
        responses[f"repos/o/r/actions/runs/{run_id}/jobs"] = body
    return responses


@unittest.skipUnless(shutil.which("jq"), "jq is needed to apply require_ci_green.sh's --jq filters")
class RequireCiGreenTest(unittest.TestCase):
    def check(self, responses):
        """Run require_ci_green.sh against a fake `gh` that answers only the exact URLs
        in `responses` (anything else fails) and applies the script's own --jq filter."""
        with tempfile.TemporaryDirectory() as bin_dir:
            gh = Path(bin_dir) / "gh"
            gh.write_text(
                "#!/bin/sh\n"
                "while [ $# -gt 0 ]; do\n"
                '  case "$1" in --jq) filter="$2"; shift ;; api|--paginate) ;; *) url="$1" ;; esac\n'
                "  shift\n"
                "done\n"
                'printf "%s" "$FAKE_API" | jq -e --arg u "$url" \'has($u)\' > /dev/null || '
                '{ echo "unexpected $url" >&2; exit 1; }\n'
                'printf "%s" "$FAKE_API" | jq --arg u "$url" \'.[$u]\' | jq -r "$filter"\n'
            )
            gh.chmod(0o755)
            env = {**os.environ, "PATH": f"{bin_dir}{os.pathsep}{os.environ['PATH']}",
                   "FAKE_API": json.dumps(responses)}
            result = subprocess.run(["bash", HERE / "require_ci_green.sh", "o/r", "abc"],
                                    capture_output=True, text=True, env=env)
            return result.returncode, result.stdout

    def run_of(self, run_id, status="completed"):
        return {"id": run_id, "status": status}

    def test_all_jobs_success_passes(self):
        self.assertEqual(self.check(api([self.run_of(1)], {1: jobs("success")}))[0], 0)

    def test_newest_run_wins(self):
        responses = api([self.run_of(1), self.run_of(2)], {1: jobs("failure"), 2: jobs("success")})
        self.assertEqual(self.check(responses)[0], 0)
        responses = api([self.run_of(1), self.run_of(2)], {1: jobs("success"), 2: jobs("failure")})
        self.assertNotEqual(self.check(responses)[0], 0)

    def test_anything_else_fails(self):
        cases = {
            "no ci.yml run": api([], {}),
            "job missing from a finished run": api([self.run_of(1)], {1: jobs("success", names=CHECKS[:2])}),
            "running": api([self.run_of(1, "in_progress")], {1: jobs(None, "in_progress")}),
            "failure": api([self.run_of(1)], {1: jobs("failure")}),
            "cancelled": api([self.run_of(1)], {1: jobs("cancelled")}),
            # Run 2 belongs to another workflow: it is not among ci.yml's runs.
            "another workflow's success, ci.yml failed": api(
                [self.run_of(1)], {1: jobs("failure"), 2: jobs("success")}),
        }
        for name, responses in cases.items():
            with self.subTest(name):
                self.assertNotEqual(self.check(responses)[0], 0)

    def test_messages(self):
        self.assertIn("has not run", self.check(api([], {}))[1])
        self.assertIn("still running",
                      self.check(api([self.run_of(1, "in_progress")], {1: {"jobs": []}}))[1])
        self.assertIn("re-run CI", self.check(api([self.run_of(1)], {1: jobs("cancelled")}))[1])
        self.assertIn("fix it on main", self.check(api([self.run_of(1)], {1: jobs("failure")}))[1])


if __name__ == "__main__":
    unittest.main()
