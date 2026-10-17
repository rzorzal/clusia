#!/usr/bin/env bash
# Usage: render.sh OUT_DIR PNG_DIR [page …]   (pages default to index; HEIGHT sets the window height)
# Assembles the site into OUT_DIR and screenshots each page at 12h, 18h and 23h, 1440 and 390 px wide,
# as PNG_DIR/<page>-<hour>h-<width>.png. Pages are served over http on 127.0.0.1 because Chrome refuses
# ES module scripts from file:// URLs.
set -euo pipefail

out="${1:?usage: render.sh OUT_DIR PNG_DIR [page …]}"
png="${2:?usage: render.sh OUT_DIR PNG_DIR [page …]}"
shift 2
pages=("$@")
[ "${#pages[@]}" -gt 0 ] || pages=(index)
height="${HEIGHT:-2400}"
chrome="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

bash "$root/site/tools/assemble.sh" "$out"
mkdir -p "$png"
png="$(cd "$png" && pwd)"

port="$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"
profile="$(mktemp -d)"
# The server root holds the site (as site/) and a frame page for widths under 500 px; one origin for both.
serve="$(mktemp -d)"
ln -s "$(cd "$out" && pwd)" "$serve/site"
cat > "$serve/frame.html" <<'HTML'
<!doctype html>
<body style="margin:0">
<iframe style="border:0;display:block;margin:0 auto"></iframe>
<script>
  const q = new URLSearchParams(location.search);
  const f = document.querySelector('iframe');
  f.style.width = q.get('w') + 'px';
  f.style.height = q.get('h') + 'px';
  f.src = q.get('src');
</script>
HTML
python3 -m http.server "$port" --bind 127.0.0.1 --directory "$serve" >/dev/null 2>&1 &
server=$!
trap 'kill "$server" 2>/dev/null || true; wait "$server" 2>/dev/null || true; rm -rf "$profile" "$serve"' EXIT

for _ in $(seq 50); do
  curl -fsS -o /dev/null "http://127.0.0.1:$port/frame.html" 2>/dev/null && break
  sleep 0.1
done

for page in "${pages[@]}"; do
  for hour in 12 18 23; do
    for width in 1440 390; do
      shot="$png/$page-${hour}h-$width.png"
      rm -f "$shot"
      url="http://127.0.0.1:$port/site/$page.html?hour=$hour"
      window="$width"
      # Headless Chrome never makes a window narrower than 500 px, so a narrower page is laid out in a
      # centred iframe of the exact width and the shot is cropped to it afterwards.
      if [ "$width" -lt 500 ]; then
        window=500
        url="http://127.0.0.1:$port/frame.html?w=$width&h=$height&src=$(python3 -c 'import sys, urllib.parse; print(urllib.parse.quote(sys.argv[1], safe=""))' "/site/$page.html?hour=$hour")"
      fi
      # A fresh profile keeps headless Chrome away from a Chrome the user has open; the virtual time
      # budget lets the web font and the module script finish before the shot.
      "$chrome" --headless=new --force-prefers-reduced-motion --hide-scrollbars \
        --no-first-run --no-default-browser-check --user-data-dir="$profile" --virtual-time-budget=4000 \
        --window-size="$window,$height" --screenshot="$shot" "$url" >/dev/null 2>&1 &
      browser=$!
      # Chrome can write the screenshot and then never exit (seen on macOS with the screen locked),
      # so wait for the file instead of the process.
      for _ in $(seq 600); do
        [ -s "$shot" ] && break
        kill -0 "$browser" 2>/dev/null || break
        sleep 0.1
      done
      sleep 0.3
      kill "$browser" 2>/dev/null || true
      wait "$browser" 2>/dev/null || true
      [ -s "$shot" ] || { echo "render: no screenshot for $page at ${hour}h, $width px" >&2; exit 1; }
      # sips crops around the centre, where the frame page puts the iframe.
      [ "$window" = "$width" ] || sips -c "$height" "$width" "$shot" >/dev/null
      echo "$shot"
    done
  done
done
