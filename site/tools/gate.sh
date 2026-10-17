#!/usr/bin/env bash
# Unit tests, then assemble the site into a scratch directory and check what would be deployed.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"

node --test site/tests/*.test.mjs

out="$(mktemp -d)"
trap 'rm -rf "$out"' EXIT
bash site/tools/assemble.sh "$out"
node site/tools/check.mjs "$out"
