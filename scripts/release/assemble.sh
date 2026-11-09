#!/usr/bin/env bash
# Moves each build job's files into <dist-dir> (zips) and <rustc-dir> (rustc-*.txt),
# failing unless <artifacts-dir> holds exactly one release-<platform> artifact per
# platform, each containing only its own cw_tari_wallet-<tag>-<platform>.zip and
# rustc-<platform>.txt. This stops one build job from replacing another platform's
# zip before SHA256SUMS is computed.
# Usage: assemble.sh <tag> <artifacts-dir> <dist-dir> <rustc-dir> <platform>...
set -euo pipefail
tag="$1"
artifacts="$2"
dist="$3"
rustc_dir="$4"
shift 4
mkdir -p "$dist" "$rustc_dir"

# Every file and directory under $1 (recursively), sorted.
listing() { (cd "$1" && find . -mindepth 1 | sed 's|^\./||' | LC_ALL=C sort); }

expected_dirs="$(printf 'release-%s\n' "$@" | LC_ALL=C sort)"
actual_dirs="$(cd "$artifacts" && find . -mindepth 1 -maxdepth 1 | sed 's|^\./||' | LC_ALL=C sort)"
if [ "$actual_dirs" != "$expected_dirs" ]; then
  echo "::error::expected exactly these artifacts: $(tr '\n' ' ' <<<"$expected_dirs")" >&2
  exit 1
fi
for platform in "$@"; do
  dir="$artifacts/release-$platform"
  zip="cw_tari_wallet-$tag-$platform.zip"
  expected="$(printf '%s\n' "$zip" "rustc-$platform.txt" | LC_ALL=C sort)"
  if [ "$(listing "$dir")" != "$expected" ]; then
    echo "::error::release-$platform must contain only $zip and rustc-$platform.txt" >&2
    exit 1
  fi
  mv "$dir/$zip" "$dist/"
  mv "$dir/rustc-$platform.txt" "$rustc_dir/"
done
