import unittest
from pathlib import Path

from release_notes import checked_rustc, locked_versions, notes, platform_of, rustc_summary, size_rows

LOCK = """
[[package]]
name = "flutter_rust_bridge"
version = "2.11.1"

[[package]]
name = "minotari-wallet-core"
version = "0.2.0"
source = "git+https://github.com/tari-project/minotari-cli.git?rev=509e6b3#509e6b3bf9f2"

[[package]]
name = "tari_common"
version = "6.3.0-pre.1"

[[package]]
name = "tari_crypto"
version = "0.24.1"
"""


class ReleaseNotesTest(unittest.TestCase):
    def test_locked_versions(self):
        tari, rev, frb = locked_versions(LOCK)
        self.assertEqual(tari, [("tari_common", "6.3.0-pre.1"), ("tari_crypto", "0.24.1")])
        self.assertEqual(rev, "509e6b3bf9f2")
        self.assertEqual(frb, "2.11.1")

    def test_real_lockfile_parses(self):
        lock = Path(__file__).resolve().parents[2] / "Cargo.lock"
        tari, rev, frb = locked_versions(lock.read_text())
        self.assertIn("tari_common", {name for name, _ in tari})
        self.assertEqual(len(rev), 40)
        self.assertTrue(frb)

    def test_crate_locked_at_two_versions_lists_both(self):
        lock = LOCK + '\n[[package]]\nname = "tari_crypto"\nversion = "0.23.3"\n'
        text = notes("v0.2.0", "abc123", {"ios": "rustc 1.90.0"}, lock, {}, None, {})
        self.assertIn("- tari_crypto: `0.23.3`", text)
        self.assertIn("- tari_crypto: `0.24.1`", text)

    def test_rustc_summary(self):
        self.assertEqual(rustc_summary({"ios": "rustc 1.90.0", "linux": "rustc 1.90.0"}), "`rustc 1.90.0`")
        self.assertEqual(
            rustc_summary({"linux": "rustc 1.91.0", "ios": "rustc 1.90.0"}),
            "ios: `rustc 1.90.0`, linux: `rustc 1.91.0`",
        )

    def test_checked_rustc(self):
        self.assertEqual(checked_rustc("rustc 1.99.0 (b940084d7 2026-09-28)\n"),
                         "rustc 1.99.0 (b940084d7 2026-09-28)")
        self.assertEqual(checked_rustc("rustc 1.100.0-nightly (787af2b8c 2026-08-25)"),
                         "rustc 1.100.0-nightly (787af2b8c 2026-08-25)")
        for bad in ["", "rustc 1.99.0", "rustc 1.99.0 (b940084d7 2026-09-28)\n::error::x",
                    "rustc 1.99.0 (b940084d7 2026-09-28) `injected`", "::warning::hi",
                    "rustc \u0661.99.0 (b940084d7 2026-09-28)"]:
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                checked_rustc(bad)

    def test_platform_of(self):
        self.assertEqual(platform_of("cw_tari_wallet-v0.2.0-android.zip", "v0.2.0"), "android")
        self.assertIsNone(platform_of("SHA256SUMS", "v0.2.0"))
        self.assertIsNone(platform_of("cw_tari_wallet-v0.1.0-ios.zip", "v0.2.0"))

    def test_size_rows(self):
        self.assertEqual(size_rows({"ios": 1000}, None, {}), ["| ios | 1,000 B | first release |"])
        self.assertEqual(
            size_rows({"ios": 1100, "linux": 5}, "v0.2.0", {"ios": 1000}),
            [
                "| ios | 1,100 B | +100 B (+10.0%) vs v0.2.0 |",
                "| linux | 5 B | new since v0.2.0 |",
            ],
        )
        self.assertEqual(
            size_rows({"ios": 900}, "v0.2.0", None),
            ["| ios | 900 B | no size data for v0.2.0 |"],
        )
        self.assertEqual(
            size_rows({"ios": 900}, "v0.2.0", {"ios": 1000}),
            ["| ios | 900 B | -100 B (-10.0%) vs v0.2.0 |"],
        )

    def test_notes_list_sha_and_versions(self):
        text = notes("v0.2.0", "abc123", {"ios": "rustc 1.90.0"}, LOCK, {"ios": 1}, None, {})
        for needle in ("`abc123`", "`509e6b3bf9f2`", "flutter_rust_bridge: `2.11.1`",
                       "tari_crypto: `0.24.1`", "rustc: `rustc 1.90.0`",
                       "sha256sum -c --ignore-missing SHA256SUMS",
                       "--cert-identity https://github.com/tari-project/cw_tari_wallet/"
                       ".github/workflows/release.yml@refs/tags/v0.2.0",
                       "--source-digest abc123 --deny-self-hosted-runners", "| ios | 1 B | first release |"):
            self.assertIn(needle, text)


if __name__ == "__main__":
    unittest.main()
