#!/usr/bin/env bash
# Fails unless CI's codegen-drift, forbidden-crates and cargo-deny checks succeeded
# on <sha>, so a release never ships Dart bindings that are stale against its Rust.
# (`public API stability` runs only on pull requests, so it never runs on main.)
# Usage: require_ci_green.sh <owner/repo> <sha>   (needs GH_TOKEN with checks: read)
set -euo pipefail
repo="$1"
sha="$2"
failed=0
for check in "bridge codegen drift" "forbidden crates" "cargo-deny"; do
  # Only GitHub Actions runs count, so another app cannot post a look-alike check.
  # Each run gives its conclusion, or its status if it has not finished.
  states="$(gh api --paginate --method GET "repos/$repo/commits/$sha/check-runs" \
    -f check_name="$check" \
    --jq '.check_runs[] | select(.app.slug == "github-actions") | .conclusion // .status')"
  if [ -z "$states" ]; then
    echo "::error::CI check '$check' has not run on $sha; wait for CI on that commit, then re-run."
    failed=1
  elif grep -qxE 'queued|in_progress|pending|waiting|requested' <<<"$states"; then
    echo "::error::CI check '$check' is still running on $sha; wait for it to finish, then re-run."
    failed=1
  elif grep -qxE 'failure|timed_out|action_required|startup_failure' <<<"$states"; then
    echo "::error::CI check '$check' failed on $sha; fix it on main first."
    failed=1
  elif grep -qvx success <<<"$states"; then
    # cancelled, skipped, stale, neutral: CI did not finish; nothing to fix.
    echo "::error::CI check '$check' did not complete on $sha ($(sort -u <<<"$states" | tr '\n' ' ')); re-run CI on $sha, then re-run."
    failed=1
  else
    echo "ok: $check"
  fi
done
exit $failed
