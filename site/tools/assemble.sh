#!/usr/bin/env bash
# Usage: assemble.sh OUT_DIR
# Copies site/ (minus tools/ and tests/) into OUT_DIR, then every docs/assets file the pages and
# stylesheets reference as assets/<path>, so the screenshots live in one place in the repo.
set -euo pipefail

out="${1:?usage: assemble.sh OUT_DIR}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

mkdir -p "$out"
(cd "$root/site" && tar --exclude=./tools --exclude=./tests -cf - .) | (cd "$out" && tar -xf -)

refs="$(
  { find "$root/site" -type f \( -name '*.html' -o -path "$root/site/css/*.css" \) \
      -not -path "$root/site/tools/*" -not -path "$root/site/tests/*" -print0 \
      | xargs -0 grep -ohE 'assets/[^"'"'"') ,]+' 2>/dev/null || true; } | sort -u
)"

missing=0
while IFS= read -r ref; do
  [ -n "$ref" ] || continue
  src="$root/docs/$ref"
  if [ -f "$src" ]; then
    mkdir -p "$out/$(dirname "$ref")"
    cp "$src" "$out/$ref"
  else
    echo "assemble: missing asset $ref (expected at docs/$ref)" >&2
    missing=1
  fi
done <<< "$refs"

exit "$missing"
