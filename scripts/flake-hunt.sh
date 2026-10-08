#!/usr/bin/env bash
# Runs the suites that talk to a daemon over and over and says which tests failed.
# Usage: scripts/flake-hunt.sh [RUNS]   (default 20)
set -u
runs="${1:-20}"
out="$(mktemp -d)"
failed=0
for ((i = 1; i <= runs; i++)); do
  if ! cargo test --no-fail-fast -p clusiad -p clusia-protocol -p clusia-app -p clusia \
    >"$out/run-$i.log" 2>&1; then
    failed=$((failed + 1))
    echo "run $i failed:"
    grep -E '^test .* FAILED|panicked at' "$out/run-$i.log" | sort -u
  fi
done
echo "$failed of $runs runs failed; logs in $out"
[ "$failed" -eq 0 ]
