#!/usr/bin/env bash
# Fails unless CI's codegen-drift, forbidden-crates and cargo-deny jobs succeeded on
# <sha>, so a release never ships Dart bindings that are stale against its Rust.
# (`public API stability` runs only on pull requests, so it never runs on main.)
# Usage: require_ci_green.sh <owner/repo> <sha>   (needs GH_TOKEN with actions: read)
set -euo pipefail
repo="$1"
sha="$2"

# Only ci.yml's own run for a push of <sha> to main counts: a branch workflow can
# post look-alike check runs, but it cannot become this run. The newest such run wins.
run="$(gh api --paginate "repos/$repo/actions/workflows/ci.yml/runs?head_sha=$sha&event=push&branch=main" \
  --jq '.workflow_runs[] | "\(.id) \(.status)"' | sort -n | tail -n 1)"
if [ -z "$run" ]; then
  echo "::error::CI (ci.yml) has not run on $sha as a push to main; wait for CI on that commit, then re-run."
  exit 1
fi
run_id="${run%% *}"
run_status="${run#* }"
# Jobs of the run's latest attempt (the endpoint's default), so a re-run replaces a
# failed attempt.
jobs="$(gh api --paginate "repos/$repo/actions/runs/$run_id/jobs" \
  --jq '.jobs[] | "\(.conclusion // .status)\t\(.name)"')"

failed=0
for check in "bridge codegen drift" "forbidden crates" "cargo-deny"; do
  states="$(awk -F'\t' -v name="$check" '$2 == name { print $1 }' <<<"$jobs")"
  if [ -z "$states" ] && [ "$run_status" != completed ]; then
    echo "::error::CI job '$check' is still running on $sha; wait for it to finish, then re-run."
    failed=1
  elif [ -z "$states" ]; then
    echo "::error::CI job '$check' has not run on $sha; wait for CI on that commit, then re-run."
    failed=1
  elif grep -qxE 'queued|in_progress|pending|waiting|requested' <<<"$states"; then
    echo "::error::CI job '$check' is still running on $sha; wait for it to finish, then re-run."
    failed=1
  elif grep -qxE 'failure|timed_out|action_required|startup_failure' <<<"$states"; then
    echo "::error::CI job '$check' failed on $sha; fix it on main first."
    failed=1
  elif grep -qvx success <<<"$states"; then
    # cancelled, skipped, stale, neutral: CI did not finish; nothing to fix.
    echo "::error::CI job '$check' did not complete on $sha ($(sort -u <<<"$states" | tr '\n' ' ')); re-run CI on $sha, then re-run."
    failed=1
  else
    echo "ok: $check"
  fi
done
exit $failed
