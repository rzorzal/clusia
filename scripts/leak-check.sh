#!/usr/bin/env bash
# Runs the whole test suite and fails when it leaves a clusiad started on a temporary home
# running. Extra arguments go to `cargo test --workspace`.
set -u
tmp="${TMPDIR:-/tmp}"
tmp="${tmp%/}"
leftover() {
  pgrep -f "clusiad --home (/private)?($tmp|/tmp|/var/folders)/" | sort
}
before="$(leftover)"
cargo test --workspace "$@"
status=$?
# A daemon told to stop at the end of a test may still be on its way out.
for _ in $(seq 50); do
  leaked="$(comm -13 <(echo "$before") <(leftover))"
  [ -z "$leaked" ] && break
  sleep 0.2
done
if [ -n "$leaked" ]; then
  echo "clusiad processes left behind by the tests:"
  ps -o pid,etime,command -p "$(echo $leaked | tr ' ' ,)"
  exit 1
fi
echo "no clusiad left behind"
exit "$status"
