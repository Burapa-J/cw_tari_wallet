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
  # The API returns only the latest run of each check (filter=latest), so a
  # re-run replaces the earlier result: its conclusion, or its status if unfinished.
  state="$(gh api --paginate --method GET "repos/$repo/commits/$sha/check-runs" \
    -f check_name="$check" --jq '.check_runs[] | .conclusion // .status')"
  state="${state%%$'\n'*}"
  case "$state" in
    success)
      echo "ok: $check" ;;
    "")
      echo "::error::CI check '$check' has not run on $sha; wait for CI on that commit, then re-run."
      failed=1 ;;
    queued | in_progress | pending | waiting | requested)
      echo "::error::CI check '$check' is still running on $sha; wait for it to finish, then re-run."
      failed=1 ;;
    failure | timed_out | action_required | startup_failure)
      echo "::error::CI check '$check' failed on $sha ($state); fix it on main first."
      failed=1 ;;
    *)
      # cancelled, skipped, stale, neutral: CI did not finish; nothing to fix.
      echo "::error::CI check '$check' did not complete on $sha ($state); re-run CI on $sha, then re-run."
      failed=1 ;;
  esac
done
exit $failed
