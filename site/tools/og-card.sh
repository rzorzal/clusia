#!/usr/bin/env bash
# Usage: og-card.sh
# Renders site/tools/og-card.html at 1200x630 into docs/assets/site-og.png. The page is served over http on
# 127.0.0.1 from the repository root, like render.sh, so it loads the web font and the repo images the same way.
set -euo pipefail

chrome="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
final="$root/docs/assets/site-og.png"
shot="$(mktemp -d)/site-og.png"

port="$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"
profile="$(mktemp -d)"
python3 -m http.server "$port" --bind 127.0.0.1 --directory "$root" >/dev/null 2>&1 &
server=$!
trap 'rm -rf "$(dirname "$shot")"; kill "$server" 2>/dev/null || true; wait "$server" 2>/dev/null || true; rm -rf "$profile"' EXIT

for _ in $(seq 50); do
  curl -fsS -o /dev/null "http://127.0.0.1:$port/site/tools/og-card.html" 2>/dev/null && break
  sleep 0.1
done

rm -f "$shot"
# Chrome can write the screenshot and then never exit, so wait for the file instead of the process.
"$chrome" --headless=new --hide-scrollbars --no-first-run --no-default-browser-check --user-data-dir="$profile" \
  --virtual-time-budget=4000 --window-size=1200,630 --screenshot="$shot" \
  "http://127.0.0.1:$port/site/tools/og-card.html" >/dev/null 2>&1 &
browser=$!
for _ in $(seq 600); do
  [ -s "$shot" ] && break
  kill -0 "$browser" 2>/dev/null || break
  sleep 0.1
done
sleep 0.3
kill "$browser" 2>/dev/null || true
wait "$browser" 2>/dev/null || true
[ -s "$shot" ] || { echo "og-card: no screenshot" >&2; exit 1; }
# The old card is replaced only once a new one exists.
mv "$shot" "$final"
echo "$final"
