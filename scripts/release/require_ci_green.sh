#!/usr/bin/env bash
# Fails unless CI's codegen-drift and forbidden-crates checks succeeded on <sha>,
# so a release never ships Dart bindings that are stale against its Rust.
# (`public API stability` runs only on pull requests, so it never runs on main.)
# Usage: require_ci_green.sh <owner/repo> <sha>   (needs GH_TOKEN with checks: read)
set -euo pipefail
repo="$1"
sha="$2"
failed=0
for check in "bridge codegen drift" "forbidden crates"; do
  # One line per run of that check (re-runs add lines): its conclusion, or its
  # status while it is still queued or running.
  states="$(gh api --paginate --method GET "repos/$repo/commits/$sha/check-runs" \
    -f check_name="$check" --jq '.check_runs[] | .conclusion // .status')"
  if grep -qx success <<<"$states"; then
    echo "ok: $check"
  elif [ -z "$states" ]; then
    echo "::error::CI check '$check' has not run on $sha; wait for CI on that commit, then re-run."
    failed=1
  elif grep -qxE 'queued|in_progress|pending|waiting|requested' <<<"$states"; then
    echo "::error::CI check '$check' is still running on $sha; wait for it to finish, then re-run."
    failed=1
  else
    echo "::error::CI check '$check' did not succeed on $sha ($(tr '\n' ' ' <<<"$states")); fix it on main first."
    failed=1
  fi
done
exit $failed
