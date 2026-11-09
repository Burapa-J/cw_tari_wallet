"""Write the GitHub release notes for a cw_tari_wallet tag to stdout.

Usage: release_notes.py <tag> <commit-sha> <rustc-dir> <dist-dir> [<previous-assets.json>]

<rustc-dir> holds one rustc-<platform>.txt (`rustc -V` output) per build job.

<previous-assets.json> is `gh release view <prev-tag> --json tagName,assets`; it
feeds the size delta and is omitted for the first release. `"assets": null` means
the previous release could not be read.
"""

import json
import sys
import tomllib
from pathlib import Path

PREFIX = "cw_tari_wallet-"


def locked_versions(lock_text):
    """Return (sorted tari_* (name, version) pairs, minotari-wallet-core rev, FRB version).

    Pairs, not a dict: a crate locked at two versions must list both.
    """
    packages = tomllib.loads(lock_text)["package"]
    tari = sorted({(p["name"], p["version"]) for p in packages if p["name"].startswith("tari_")})
    core = next(p for p in packages if p["name"] == "minotari-wallet-core")
    frb = next(p for p in packages if p["name"] == "flutter_rust_bridge")
    return tari, core["source"].rsplit("#", 1)[1], frb["version"]


def platform_of(asset_name, tag):
    """`cw_tari_wallet-v0.2.0-android.zip` -> `android` (None for other files)."""
    head = f"{PREFIX}{tag}-"
    if asset_name.startswith(head) and asset_name.endswith(".zip"):
        return asset_name[len(head) : -len(".zip")]
    return None


def rustc_summary(versions):
    """`platform -> rustc -V`: one value if every job agreed, else one per platform."""
    if len(set(versions.values())) == 1:
        return f"`{next(iter(versions.values()))}`"
    return ", ".join(f"{platform}: `{v}`" for platform, v in sorted(versions.items()))


def size_rows(current, previous_tag, previous):
    """Markdown table rows; `current`/`previous` map platform -> bytes.

    `previous` is None when the previous release's sizes could not be read.
    """
    rows = []
    for platform in sorted(current):
        size = current[platform]
        if previous_tag is None:
            delta = "first release"
        elif previous is None:
            delta = f"no size data for {previous_tag}"
        elif platform not in previous:
            delta = f"new since {previous_tag}"
        else:
            diff = size - previous[platform]
            delta = f"{diff:+,} B ({diff / previous[platform]:+.1%}) vs {previous_tag}"
        rows.append(f"| {platform} | {size:,} B | {delta} |")
    return rows


def notes(tag, sha, rustc, lock_text, current, previous_tag, previous):
    tari, core_rev, frb = locked_versions(lock_text)
    lines = [
        f"Commit: `{sha}`",
        "",
        "Prebuilt native libraries for Cake Wallet. Verify downloaded zips with "
        "`sha256sum -c --ignore-missing SHA256SUMS` "
        "(macOS: `shasum -a 256 -c --ignore-missing SHA256SUMS`). The zips and "
        "SHA256SUMS carry GitHub build-provenance attestations; check one with "
        "`gh attestation verify <zip> --repo tari-project/cw_tari_wallet`.",
        "",
        "**Network:** every library is a mainnet-group build "
        "(`TARI_TARGET_NETWORK=mainnet`), so it accepts only `MainNet` and "
        "`StageNet`, chosen at runtime. Esmeralda needs a local build with "
        "`TARI_TARGET_NETWORK=testnet`.",
        "",
        f"**Dart bindings:** `{PREFIX}{tag}-dart.zip` holds the FRB-generated Dart "
        "code from this commit; use it with these libraries only.",
        "",
        "## Versions",
        "",
        f"- minotari-wallet-core rev: `{core_rev}`",
        f"- flutter_rust_bridge: `{frb}`",
        f"- rustc: {rustc_summary(rustc)}",
    ]
    lines += [f"- {name}: `{version}`" for name, version in tari]
    lines += ["", "## Artifact sizes", "", "| Platform | Size | Change |", "|---|---|---|"]
    lines += size_rows(current, previous_tag, previous)
    return "\n".join(lines) + "\n"


def main(argv):
    tag, sha, dist = argv[1], argv[2], Path(argv[4])
    rustc = {
        p.stem.removeprefix("rustc-"): p.read_text().strip()
        for p in Path(argv[3]).glob("rustc-*.txt")
    }
    current = {
        platform_of(p.name, tag): p.stat().st_size
        for p in dist.iterdir()
        if platform_of(p.name, tag)
    }
    previous_tag, previous = None, {}
    if len(argv) > 5:
        release = json.loads(Path(argv[5]).read_text())
        previous_tag = release["tagName"]
        if release["assets"] is None:
            previous = None
        else:
            for asset in release["assets"]:
                platform = platform_of(asset["name"], previous_tag)
                if platform:
                    previous[platform] = asset["size"]
    lock_text = Path("Cargo.lock").read_text()
    sys.stdout.write(notes(tag, sha, rustc, lock_text, current, previous_tag, previous))


if __name__ == "__main__":
    main(sys.argv)
