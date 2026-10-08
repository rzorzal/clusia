#!/usr/bin/env bash
# Owner check for installing, launching and notifications. Run it on the real Mac with someone
# at the keyboard: macOS asks about notifications, and a few things can only be seen.
#
# It stops the daemon and kills the tray and the window (the open, crash and reinstall steps),
# so run it when you are not in the middle of a review. It never uses admin rights, never
# deletes files and never unloads the LaunchAgent.
#
#   scripts/m6-check.sh                    build, install, then check everything
#   scripts/m6-check.sh --skip-install     check what is installed
#   scripts/m6-check.sh --after-login      after logging out and in again
#   scripts/m6-check.sh --brew PREFIX      the Homebrew formula, in a Homebrew kept in PREFIX
#   scripts/m6-check.sh --list             print the steps and exit
#   add --yes to skip the question below, for a run without a terminal
#
# The runs that stop or reinstall Clusia ask once, up front; --yes answers for them.
set -u

LABEL="io.github.rzorzal.clusia.daemon"
BUNDLE_ID="io.github.rzorzal.clusia"
ROOT="${CLUSIA_HOME:-$HOME/Library/Application Support/Clusia}"
SOCKET="$ROOT/clusiad.sock"
LOGS="$HOME/Library/Logs/Clusia"
UID_NUM="$(id -u)"
REPO="$(cd "$(dirname "$0")/.." && pwd)"
FLAKE_RUNS="${CLUSIA_FLAKE_RUNS:-20}"
passed=0
failed=0
APP=""
ASSUME_YES=0

STEPS=(
  "install: build, install, and check the app bundle, the ad hoc signature, the LaunchAgent and the CLI link"
  "open: with nothing running, opening Clusia.app brings up the tray, the daemon and the window"
  "second-launch: opening the app again brings the window forward, leaves one tray, and tray.log shows no warnings"
  "terminal: clusia works in a new login shell"
  "crash: a killed daemon comes back"
  "notification: a test notification plays a soft sound, and clicking it opens Clusia; Play makes a real sound"
  "reinstall: installing again keeps the notification permission and does not ask for it again"
  "icons: the Clusia icon shows everywhere macOS shows the app, including the Dock and Cmd-Tab"
  "hardening: the test suite leaves no daemon behind, and the flake hunt is clean while a build runs"
  "login: after logging out and in, the tray is back (run with --after-login)"
)

say() { printf '\n%s\n' "$1"; }
pass() { passed=$((passed + 1)); printf '  PASS  %s\n' "$1"; }
fail() { failed=$((failed + 1)); printf '  FAIL  %s\n' "$1"; }

check() {
  local what="$1"
  shift
  if "$@" >/dev/null 2>&1; then pass "$what"; else fail "$what"; fi
}

wait_for() {
  local what="$1" secs="$2" i=0
  shift 2
  while [ "$i" -lt "$secs" ]; do
    if "$@" >/dev/null 2>&1; then
      pass "$what"
      return 0
    fi
    sleep 1
    i=$((i + 1))
  done
  fail "$what (waited ${secs}s)"
  return 1
}

ask() {
  local answer
  read -r -p "  ? $1 [y/n] " answer
  case "$answer" in y | Y | yes) pass "$1" ;; *) fail "$1" ;; esac
}

# Everything below the up-front question stops or reinstalls the real Clusia.
confirm_disruption() {
  local answer
  [ "$ASSUME_YES" = 1 ] && return 0
  echo "This stops the Clusia daemon, tray and window and reinstalls over the real app."
  printf 'Continue? [y/N] '
  read -r answer || answer=n
  case "$answer" in y | Y | yes) return 0 ;; *) return 1 ;; esac
}

permission_allowed() { send '"daemon_status"' | grep -q llowed; }

running() { pgrep -x "$1" >/dev/null; }
stopped() { ! pgrep -x "$1" >/dev/null; }
replaced() {
  local now
  now="$(pgrep -x clusiad | head -1)"
  [ -n "$now" ] && [ "$now" != "$1" ]
}
all_stopped() { stopped clusiad && stopped clusia-tray && stopped clusia-app; }
one_tray() { [ "$(pgrep -x clusia-tray | wc -l | tr -d ' ')" = 1 ]; }
is_adhoc() { codesign -dv "$1" 2>&1 | grep -q 'Signature=adhoc'; }

app_path() {
  local dir
  for dir in /Applications "$HOME/Applications"; do
    if [ -d "$dir/Clusia.app" ]; then
      echo "$dir/Clusia.app"
      return 0
    fi
  done
  return 1
}

plist_value() { /usr/libexec/PlistBuddy -c "Print :$2" "$1" 2>/dev/null; }

# Sends one command to the daemon over its socket and prints what comes back.
send() {
  {
    printf '%s\n' '{"type":"hello","protocol":1,"client":"m6-check","version":"0"}'
    printf '{"type":"request","id":1,"cmd":%s}\n' "$1"
    sleep 1
  } | nc -U "$SOCKET" 2>/dev/null
}

stop_everything() {
  [ -n "$APP" ] && "$APP/Contents/MacOS/clusia" daemon stop >/dev/null 2>&1
  pkill -x clusia-app 2>/dev/null
  pkill -x clusia-tray 2>/dev/null
  wait_for "nothing of Clusia is running" 15 all_stopped
}

install_app() {
  (cd "$REPO" && cargo run --release -p clusia --quiet -- install)
}

step_install() {
  say "1. Install"
  if install_app; then
    pass "clusia install finished"
  else
    fail "clusia install failed"
    return
  fi
  if ! APP="$(app_path)"; then
    fail "Clusia.app is in /Applications or ~/Applications"
    return
  fi
  pass "Clusia.app is at $APP"
  local info="$APP/Contents/Info.plist" agent="$HOME/Library/LaunchAgents/$LABEL.plist" name
  check "the main executable is clusia-tray" test "$(plist_value "$info" CFBundleExecutable)" = clusia-tray
  check "the bundle id is $BUNDLE_ID" test "$(plist_value "$info" CFBundleIdentifier)" = "$BUNDLE_ID"
  check "the app has no Dock icon of its own (LSUIElement)" test "$(plist_value "$info" LSUIElement)" = true
  check "the icon is in the bundle" test -f "$APP/Contents/Resources/Clusia.icns"
  for name in leaf drop chime tick; do
    check "the $name sound is in the bundle" test -f "$APP/Contents/Resources/$name.aiff"
  done
  for name in clusia clusiad clusia-tray clusia-app; do
    check "$name is in the bundle" test -x "$APP/Contents/MacOS/$name"
  done
  check "the signature is valid" codesign --verify --deep --strict "$APP"
  check "the signature is ad hoc (no keychain identity, no keychain prompt)" is_adhoc "$APP"
  check "the LaunchAgent is a valid plist" plutil -lint "$agent"
  check "the LaunchAgent restarts the daemon only after a crash" test "$(plist_value "$agent" KeepAlive:SuccessfulExit)" = false
  check "launchd has loaded the LaunchAgent" launchctl print "gui/$UID_NUM/$LABEL"
}

step_open() {
  say "2. Opening the app"
  stop_everything
  open -b "$BUNDLE_ID" || open "$APP"
  wait_for "the tray is running" 30 running clusia-tray
  wait_for "the daemon is running" 30 running clusiad
  wait_for "the window is open" 30 running clusia-app
  ask "Is the Clusia window in front of you, showing Home or the first run?"
}

step_second_launch() {
  say "3. Opening the app a second time"
  echo "  Put another app in front of the Clusia window first."
  read -r -p "  Press return when Clusia is behind another window " _
  open -b "$BUNDLE_ID" || open "$APP"
  sleep 3
  ask "Did the Clusia window come to the front?"
  check "there is still exactly one tray" one_tray
  check "there is still one window process" test "$(pgrep -x clusia-app | wc -l | tr -d ' ')" = 1
  echo "  The second tray's warnings are only in $LOGS/tray.log:"
  if [ -f "$LOGS/tray.log" ]; then
    tail -n 15 "$LOGS/tray.log" | sed 's/^/    /'
    ask "Does tray.log show no warnings or errors from the second launch?"
  else
    fail "there is no $LOGS/tray.log"
  fi
}

step_terminal() {
  say "4. A new terminal"
  local found
  found="$(zsh -lc 'command -v clusia' 2>/dev/null)"
  if [ -n "$found" ]; then
    pass "a login shell finds clusia at $found"
  else
    fail "a login shell does not find clusia (is ~/.local/bin on the PATH?)"
  fi
  if zsh -lc 'clusia prs' >/dev/null 2>&1; then
    pass "clusia prs works in a login shell"
  else
    fail "clusia prs failed in a login shell (signed out? try: clusia auth status)"
  fi
}

step_crash() {
  say "5. A killed daemon comes back"
  "$APP/Contents/MacOS/clusia" daemon stop >/dev/null 2>&1
  wait_for "the daemon stopped cleanly" 15 stopped clusiad
  sleep 5
  check "launchd did not restart a daemon that stopped cleanly" stopped clusiad
  # The daemon the app starts after a Quit, not one started by hand, is the one that must come back.
  open -b "$BUNDLE_ID" || open "$APP"
  wait_for "the app started the daemon" 30 running clusiad
  local old
  old="$(pgrep -x clusiad | head -1)"
  kill -9 "$old"
  wait_for "a new daemon replaced pid $old" 40 replaced "$old"
  wait_for "the tray is running" 30 running clusia-tray
}

step_notification() {
  say "6. Notifications"
  echo "  macOS asks to allow notifications about 15 s after the tray starts: choose Allow."
  local reply status
  reply="$(send '"test_notification"')"
  case "$reply" in
    *'"ok":"ack"'*) pass "a tray took the test notification" ;;
    *'tray is not running'*) fail "no tray took the test notification: $reply" ;;
    *) fail "the daemon did not answer the test notification: $reply" ;;
  esac
  status="$(send '"daemon_status"')"
  case "$status" in
    *llowed*) pass "macOS permission is allowed" ;;
    *) fail "macOS permission is not allowed yet: $status" ;;
  esac
  echo "  Take a screenshot of the banner with the Clusia icon."
  ask "Did you hear a soft sound and see a Clusia banner?"
  ask "Did clicking the banner bring Clusia forward?"
  echo "  In Config > Notifications, press the play button next to Sound."
  ask "Did it play the Leaf sound out loud, and did the other sounds in the list play too?"
}

step_reinstall() {
  say "7. Installing again"
  if install_app; then
    pass "clusia install finished a second time"
  else
    fail "clusia install failed a second time"
    return
  fi
  wait_for "the tray is running" 30 running clusia-tray
  wait_for "the daemon is running" 30 running clusiad
  check "the bundle is still signed ad hoc" is_adhoc "$APP"
  wait_for "macOS permission is still allowed" 30 permission_allowed
  ask "Did macOS NOT ask for notification permission again?"
}

step_icons() {
  say "8. The icon"
  echo "  Take screenshots of: the icon in Finder, the Dock with the window open, Cmd-Tab,"
  echo "  the menu bar on a light and on a dark background, and a notification."
  ask "Is the Clusia icon shown in Finder, in /Applications and in Spotlight?"
  ask "Is it in the Dock and in Cmd-Tab while the window is open?"
  ask "Is the menu bar icon right on a light and on a dark menu bar?"
  ask "Does the notification show the Clusia icon?"
}

step_hardening() {
  say "9. Hardening"
  if (cd "$REPO" && scripts/leak-check.sh >/dev/null 2>&1); then
    pass "the test suite left no daemon behind"
  else
    fail "scripts/leak-check.sh failed (run it by hand to see why)"
  fi
  (cd "$REPO" && cargo build --workspace --quiet >/dev/null 2>&1) &
  local build=$!
  if (cd "$REPO" && scripts/flake-hunt.sh "$FLAKE_RUNS" >/dev/null 2>&1); then
    pass "no flaky test in $FLAKE_RUNS runs with a build running beside them"
  else
    fail "scripts/flake-hunt.sh $FLAKE_RUNS found a flaky test (run it by hand to see which)"
  fi
  wait "$build"
}

step_login() {
  say "After logging in"
  wait_for "the tray is back" 60 running clusia-tray
  wait_for "the daemon is back" 60 running clusiad
  ask "Is the Clusia icon in the menu bar?"
}

step_brew() {
  local prefix="$1" brew libexec out name
  brew="$prefix/bin/brew"
  say "Homebrew formula in $prefix"
  if [ ! -x "$brew" ]; then
    fail "there is no Homebrew in $prefix (git clone https://github.com/Homebrew/brew $prefix)"
    return
  fi
  if "$brew" tap rzorzal/clusia && "$brew" install --HEAD rzorzal/clusia/clusia; then
    pass "brew built Clusia from source"
  else
    fail "brew install failed"
    return
  fi
  libexec="$("$brew" --prefix clusia)/libexec/bin"
  for name in clusia clusiad clusia-tray clusia-app; do
    check "brew installed $name" test -x "$libexec/$name"
  done
  out="$("$libexec/clusia" install --dry-run --from "$libexec" 2>&1)"
  case "$out" in
    *Clusia.app*) pass "clusia install --dry-run describes the app" ;;
    *) fail "clusia install --dry-run did not mention Clusia.app: $out" ;;
  esac
}

summary() {
  printf '\n%d passed, %d failed\n' "$passed" "$failed"
  [ "$failed" -eq 0 ]
}

rest=()
for arg in "$@"; do
  if [ "$arg" = --yes ]; then ASSUME_YES=1; else rest+=("$arg"); fi
done
set -- ${rest[@]+"${rest[@]}"}

case "${1:-}" in
  --list)
    printf '%s\n' "${STEPS[@]}"
    ;;
  --after-login)
    step_login
    summary
    ;;
  --brew)
    [ -n "${2:-}" ] || {
      echo "usage: $0 --brew PREFIX" >&2
      exit 2
    }
    step_brew "$2"
    summary
    ;;
  "" | --skip-install)
    confirm_disruption || {
      echo "Nothing was stopped."
      exit 1
    }
    if [ "${1:-}" = "--skip-install" ]; then
      APP="$(app_path)" || {
        echo "Clusia.app is not installed" >&2
        exit 2
      }
    else
      step_install
    fi
    if [ -n "$APP" ]; then
      step_open
      step_second_launch
      step_terminal
      step_crash
      step_notification
      step_reinstall
      step_icons
      step_hardening
    fi
    printf '\nNext: log out and in again, then run: scripts/m6-check.sh --after-login\n'
    summary
    ;;
  *)
    echo "usage: $0 [--yes] [--list | --skip-install | --after-login | --brew PREFIX]" >&2
    exit 2
    ;;
esac
