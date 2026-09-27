#!/usr/bin/env bash
# Tests for gate.sh with a stubbed `gh`. Run: gate/test.sh
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin"
cat > "$tmp/bin/gh" <<'GH'
#!/usr/bin/env bash
# Serve canned responses in order; the last one repeats.
n=$(cat "$STATE/count" 2>/dev/null || echo 0)
file="$STATE/response.$n"
[ -f "$file" ] || file=$(ls "$STATE"/response.* | sort -t. -k2 -n | tail -1)
echo $((n + 1)) > "$STATE/count"
# `gh api ... --jq EXPR`
jq -r "${@: -1}" < "$file"
GH
chmod +x "$tmp/bin/gh"

run_case() { # name expected_result expected_skip responses...
  local name=$1 want_result=$2 want_skip=$3
  shift 3
  local state="$tmp/$name"
  mkdir -p "$state"
  local i=0
  for r in "$@"; do echo "$r" > "$state/response.$i"; i=$((i + 1)); done
  : > "$state/out"
  PATH="$tmp/bin:$PATH" STATE="$state" GITHUB_OUTPUT="$state/out" \
    BASTE_GATE_REPO=o/r BASTE_GATE_SHA=abc123 BASTE_GATE_PREFIX="baste/CI/" \
    BASTE_GATE_WAIT_MINUTES=1 BASTE_GATE_GRACE_SECONDS=0 BASTE_GATE_POLL_SECONDS=0 \
    "$here/gate.sh" > /dev/null
  grep -qx "result=$want_result" "$state/out" || { echo "FAIL $name: $(cat "$state/out")"; exit 1; }
  grep -qx "skip=$want_skip" "$state/out" || { echo "FAIL $name: $(cat "$state/out")"; exit 1; }
  echo "ok $name"
}

pass='{"statuses":[{"context":"baste/CI/build","state":"success"},{"context":"baste/CI/test (ubuntu-latest)","state":"success"},{"context":"ci/other","state":"failure"}]}'
fail='{"statuses":[{"context":"baste/CI/build","state":"success"},{"context":"baste/CI/test","state":"failure"}]}'
pending='{"statuses":[{"context":"baste/CI/build","state":"pending"}]}'
other='{"statuses":[{"context":"baste/Deploy/x","state":"success"}]}'

run_case passed passed true "$pass"
run_case failed failed false "$fail"
run_case absent absent false "$other"
run_case waits-then-passes passed true "$pending" "$pending" "$pass"
run_case error failed false '{"statuses":[{"context":"baste/CI/build","state":"error"}]}'
echo "all gate tests passed"
