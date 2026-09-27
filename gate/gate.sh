#!/usr/bin/env bash
# Decide whether a workflow's GitHub-hosted jobs can be skipped because Baste
# already reported a local pass for this commit. Reads the latest status per
# context and waits while local jobs are still pending.
set -euo pipefail

repo=${BASTE_GATE_REPO:?}
sha=${BASTE_GATE_SHA:?}
prefix=${BASTE_GATE_PREFIX:?}
wait_minutes=${BASTE_GATE_WAIT_MINUTES:-15}
grace_seconds=${BASTE_GATE_GRACE_SECONDS:-60}
poll=${BASTE_GATE_POLL_SECONDS:-10}
out=${GITHUB_OUTPUT:-/dev/stdout}

now() { date +%s; }
start=$(now)
deadline=$((start + wait_minutes * 60))
grace_end=$((start + grace_seconds))

while :; do
  # The combined status endpoint returns the latest status for each context.
  states=$(gh api "repos/$repo/commits/$sha/status?per_page=100" \
    --jq "[.statuses[] | select(.context | startswith(\"$prefix\")) | .state] | join(\" \")")
  if [ -z "$states" ]; then
    if [ "$(now)" -ge "$grace_end" ]; then result=absent; break; fi
  elif [[ " $states " == *" failure "* || " $states " == *" error "* ]]; then
    result=failed; break
  elif [[ " $states " == *" pending "* ]]; then
    if [ "$(now)" -ge "$deadline" ]; then result=pending; break; fi
  else
    result=passed; break
  fi
  sleep "$poll"
done

echo "result=$result" >> "$out"
if [ "$result" = passed ]; then
  echo "skip=true" >> "$out"
  echo "Baste reported a local pass for ${sha:0:12} ($prefix*); skipping GitHub-hosted jobs."
else
  echo "skip=false" >> "$out"
  case $result in
    absent) echo "No Baste statuses for ${sha:0:12}; running jobs on GitHub." ;;
    failed) echo "A local job failed for ${sha:0:12}; running jobs on GitHub." ;;
    pending) echo "Local jobs still pending after ${wait_minutes} minutes; running jobs on GitHub." ;;
  esac
fi
