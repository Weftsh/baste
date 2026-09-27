#!/usr/bin/env bash
# Run tests/vm/workflow.yml through the real Firecracker backend. Needs KVM,
# `sudo baste setup-network` done, and a GitHub token in $GITHUB_TOKEN (to
# download actions). Used by CI on KVM-enabled runners.
#
# Usage: scripts/firecracker-e2e.sh <static linux baste>
set -euo pipefail

BASTE=$(realpath "$1")
here=$(cd "$(dirname "$0")/.." && pwd)
W=$(mktemp -d)
echo "Work directory: $W"

# gh stand-in: Baste only needs `gh auth token`.
mkdir -p "$W/bin"
printf '#!/bin/sh\n[ "$1 $2" = "auth token" ] && echo "$GITHUB_TOKEN"\n' > "$W/bin/gh"
chmod +x "$W/bin/gh"
export BASTE_GH="$W/bin/gh"
export BASTE_BACKEND=firecracker

repo="$W/repo"
mkdir -p "$repo/.github/workflows"
cp "$here/tests/vm/workflow.yml" "$repo/.github/workflows/vm.yml"
echo committed > "$repo/marker.txt"
cd "$repo"
git init -q -b main
git remote add origin "https://github.com/${GITHUB_REPOSITORY:-weftsh/baste}.git"
git add -A
git -c user.name=ci -c user.email=ci@example.com -c commit.gpgsign=false commit -qm "vm e2e"
echo uncommitted > marker.txt

"$BASTE" doctor || true
"$BASTE" config set max_parallel_jobs 2
echo "::group::Prepare the image"
"$BASTE" image prepare
echo "::endgroup::"

set +e
"$BASTE" run --no-status
status=$?
set -e
run=$("$BASTE" status --json -n 1 | jq -r '.[0].id')
"$BASTE" status "$run"
"$BASTE" logs "$run" --no-follow > "$W/logs.txt" || true
cat "$W/logs.txt"
[ "$status" -eq 0 ] || { echo "the run failed" >&2; exit 1; }

check() { grep -q "$1" "$W/logs.txt" || { echo "missing from logs: $1" >&2; exit 1; }; }
check "user=runner"
check "root=overlay"
check "github-script sees"
check "docker works"
check "build ran on kernel"
jobs=$("$BASTE" status --json -n 1)
[ "$(jq -r '.[0].jobs[] | select(.job_id=="windows") | .state' <<<"$jobs")" = handed_to_github ]
echo "Firecracker e2e passed"
