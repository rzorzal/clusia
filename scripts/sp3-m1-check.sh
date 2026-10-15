#!/usr/bin/env bash
# Owner check for the diagrams of a change: your own agent, on a pull request of yours (a demo
# one on rzorzal/* is best), with the window in front of you. It spends some of your agent usage
# (a plan turn and one turn per diagram, up to eight), opens a real review of the pull request,
# and discards that review at the end. Nothing is published to GitHub. Clusia asks you before
# the agent runs a command, as always, and the script never answers for you.
#
#   scripts/sp3-m1-check.sh --pr rzorzal/clusia#123     run the check (asks first)
#   scripts/sp3-m1-check.sh --list                      print the steps and exit
#   add --yes to skip the question below; every step still asks you on the terminal, and
#   without one each of those questions is answered no
#
# Needs Clusia installed (clusia on the PATH, signed in to GitHub) and an agent set up in
# Config › Harness. The window is started for you; look at it when a step says so. If the pull
# request already has a review of yours on this Mac, its draft is lost at the end: use a pull
# request you do not mind opening fresh. You push one small commit to the pull request's branch
# yourself in step 5; the script pushes nothing.
set -u

PR=""
LIST=0
ASSUME_YES=0
passed=0
failed=0
ERR="$(mktemp -t sp3-m1-check.XXXXXX)" || {
  echo "Cannot create a temp file." >&2
  exit 1
}
OLD_ON_OPEN=""
OLD_SECURITY=""
OLD_AUDIT=""
OLD_DIAGRAMS=""
CHANGED=0
APP_PID=""
DRAW_PID=""
# Set to wait at most this many seconds at every step, instead of each step's own time.
WAIT_CAP="${SP3_CHECK_WAIT:-}"

restore_settings() {
  # The check changes four settings; the owner's own values come back when it ends.
  [ "$CHANGED" = 1 ] || return 0
  [ -n "$OLD_ON_OPEN" ] && clusia config set harness.on_open "$OLD_ON_OPEN" >/dev/null 2>&1
  [ -n "$OLD_SECURITY" ] && clusia config set harness.check_security "$OLD_SECURITY" >/dev/null 2>&1
  [ -n "$OLD_AUDIT" ] && clusia config set harness.audit "$OLD_AUDIT" >/dev/null 2>&1
  [ -n "$OLD_DIAGRAMS" ] && clusia config set harness.diagrams "$OLD_DIAGRAMS" >/dev/null 2>&1
  [ -n "$OLD_ON_OPEN" ] || echo "Could not read the old harness.on_open; check Config › Harness: When I open a review." >&2
  [ -n "$OLD_SECURITY" ] || echo "Could not read the old harness.check_security; check Config › Harness: Check security." >&2
  [ -n "$OLD_AUDIT" ] || echo "Could not read the old harness.audit; check Config › Harness: Audit the change." >&2
  [ -n "$OLD_DIAGRAMS" ] || echo "Could not read the old harness.diagrams; check Config › Harness: Draw diagrams." >&2
  return 0
}
# Stops a drawing the script started, so nothing keeps spending after the check ends.
stop_draw() {
  if [ -n "$DRAW_PID" ]; then
    kill -INT "$DRAW_PID" 2>/dev/null
    wait "$DRAW_PID" 2>/dev/null
    DRAW_PID=""
  fi
}
close_window() {
  if [ -n "$APP_PID" ]; then kill "$APP_PID" 2>/dev/null; fi
}
drop_err() {
  if [ -n "$ERR" ]; then rm -f "$ERR" "$ERR".*; fi
}
trap 'stop_draw; close_window; restore_settings; drop_err' EXIT
# clusia exits 130 on a Ctrl-C by status, not by the signal, so bash would go on to the next
# step; the script ends instead (the EXIT trap still cleans up).
trap 'exit 130' INT TERM HUP

STEPS=(
  "preflight: clusia is installed, the daemon answers, the agent is set up and the review opens"
  "draw: clusia diagrams --draw plans and draws, and every diagram ends ready (or says why not)"
  "view: the Diagrams tab lists them with their kind and count, and the canvas shows boxes, zoom and the legend"
  "comment: a click on a box shows its code, and Add to draft puts a comment from Diagrams into the draft"
  "push: you push a commit and the diagrams are reported out of date"
  "redraw: Redraw for the new commit makes version 2 and the older version is read-only"
  "compare: Show changes since version 1 marks what was added, changed and removed"
  "cli: clusia diagrams lists the latest version and prints it as JSON"
  "end: discarding the review deletes the drawings"
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
  local what="$1" secs="${WAIT_CAP:-$2}" i=0
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
  if yes_or_no "$1"; then pass "$1"; else fail "$1"; fi
}

# Asks a question and answers it with the status: 0 for yes. Nothing is recorded.
yes_or_no() {
  local answer
  read -r -p "  ? $1 [y/n] " answer
  case "$answer" in y | Y | yes) return 0 ;; *) return 1 ;; esac
}

usage() {
  echo "usage: $0 [--yes] [--list | --pr owner/repo#N]" >&2
}

confirm() {
  local answer
  [ "$ASSUME_YES" = 1 ] && return 0
  echo "This opens a real review of $PR and asks YOUR agent (it uses your plan) to plan and draw"
  echo "diagrams of the change: one plan turn and one turn per diagram, up to eight. You must"
  echo "answer every request for a command yourself: the script never answers for you. It changes"
  echo "four Config › Harness settings for the run and puts them back. It pushes nothing: you push"
  echo "one small commit to the pull request's branch in step 5. It discards the review of $PR on"
  echo "this Mac at the end, so a draft you have there is lost. It publishes nothing to GitHub."
  printf 'Continue? [y/N] '
  read -r answer || answer=n
  case "$answer" in y | Y | yes) return 0 ;; *) return 1 ;; esac
}

# Draft items that came from a diagram box, as the terminal prints them in JSON.
diagram_items() { clusia --json review status "$PR" 2>/dev/null | grep -Eio '"origin": ?"diagram"' | wc -l | tr -d ' '; }

# The entry lines of the latest version: `key  kind  title  state`.
entry_lines() { clusia diagrams "$PR" 2>/dev/null | grep -Ev '^(Version |$)'; }
# Whether a drawing has ended: at least one entry, none waiting or drawing.
all_settled() {
  local lines
  lines="$(entry_lines)"
  [ -n "$lines" ] && ! printf '%s\n' "$lines" | grep -Eq '[[:space:]](waiting|drawing)$'
}
version_is() { clusia diagrams "$PR" --json 2>/dev/null | grep -Eq "\"number\": ?$1[,}]"; }
is_stale() {
  clusia open "$PR" >/dev/null 2>&1
  clusia diagrams "$PR" --json 2>/dev/null | grep -Eq '"stale": ?true'
}

# Draws without a keyboard, so a request for a command waits for the window and the script can
# never answer it. `wait_draw` ends it.
draw_detached() {
  clusia diagrams "$PR" --draw </dev/null >"$ERR.out" 2>"$ERR" &
  DRAW_PID=$!
}
wait_draw() {
  local status=0
  if [ -n "$DRAW_PID" ]; then
    wait "$DRAW_PID" 2>/dev/null
    status=$?
    DRAW_PID=""
  fi
  return "$status"
}
drawing_ended() { ! kill -0 "$DRAW_PID" 2>/dev/null; }

app_binary() {
  local dir
  # Set to run another window program instead of the installed one.
  if [ -n "${SP3_CHECK_APP:-}" ]; then
    [ -x "$SP3_CHECK_APP" ] && echo "$SP3_CHECK_APP"
    return
  fi
  for dir in /Applications "$HOME/Applications"; do
    if [ -x "$dir/Clusia.app/Contents/MacOS/clusia-app" ]; then
      echo "$dir/Clusia.app/Contents/MacOS/clusia-app"
      return 0
    fi
  done
  return 1
}

start_window() {
  local app
  if app="$(app_binary)"; then
    "$app" --review "$PR" >/dev/null 2>&1 &
    APP_PID=$!
    pass "the window was started on $PR (it is closed when the check ends)"
  else
    fail "Clusia.app is installed (the window could not be started)"
  fi
}

step_preflight() {
  say "1. Preflight"
  check "clusia is installed" command -v clusia
  check "the daemon answers (it is started if needed)" clusia daemon start
  OLD_ON_OPEN="$(clusia config get harness.on_open 2>/dev/null)"
  OLD_SECURITY="$(clusia config get harness.check_security 2>/dev/null)"
  OLD_AUDIT="$(clusia config get harness.audit 2>/dev/null)"
  OLD_DIAGRAMS="$(clusia config get harness.diagrams 2>/dev/null)"
  echo "  The check turns the four boxes of 'When I open a review' off, so the only turns it runs"
  echo "  are the ones you see here. Your values (summarize '$OLD_ON_OPEN', security"
  echo "  '$OLD_SECURITY', audit '$OLD_AUDIT', diagrams '$OLD_DIAGRAMS') are put back when it ends."
  yes_or_no "Change those four settings now (they are put back when the check ends)?" || {
    echo "Nothing was changed."
    exit 1
  }
  CHANGED=1
  clusia config set harness.on_open wait >/dev/null 2>&1
  clusia config set harness.check_security false >/dev/null 2>&1
  clusia config set harness.audit false >/dev/null 2>&1
  clusia config set harness.diagrams false >/dev/null 2>&1
  check "the diagrams box is off" test "$(clusia config get harness.diagrams 2>/dev/null)" = "false"
  check "$PR is a pull request clusia can open" clusia open "$PR"
  start_window
}

step_draw() {
  local status=0
  say "2. Draw"
  echo "  This asks your agent for a plan and one drawing per entry. If it asks to run a command,"
  echo "  answer in the window: the script never does."
  draw_detached
  if wait_for "the drawing ended" 1800 drawing_ended; then
    wait_draw
    status=$?
  else
    stop_draw
    status=130
  fi
  cat "$ERR.out" 2>/dev/null
  if [ "$status" -eq 0 ]; then pass "clusia diagrams --draw finished (exit 0: every diagram was drawn)"; else fail "clusia diagrams --draw ended with exit $status (1: a diagram failed; 130: stopped): $(cat "$ERR" 2>/dev/null)"; fi
  check "every entry is settled (none waiting or drawing)" all_settled
  check "the list names the version, who drew it and the commit" sh -c 'clusia diagrams "$0" | grep -Eq "^Version [0-9]+ · drawn by .+ from [0-9a-f]{7}"' "$PR"
}

step_view() {
  say "3. The Diagrams tab"
  echo "  Look at the window and open the Diagrams tab."
  ask "Does the list show each diagram with its kind and a count (for example 'Flow · 5 boxes'), and is the first ready one selected?"
  ask "Does the canvas show boxes and arrows, the title, the kind chip and the caption, fitted to the view?"
  ask "Do − and + (or ⌘ + scroll) zoom between 25% and 200%, and does ⌘0 fit it again? Does dragging move it?"
  ask "Does the legend read Changed, New, Risk and Unchanged, and does the footer say who drew it, from which commit?"
  ask "Does every kind that was drawn look like its mockup (arrows only horizontal or vertical, no box on top of another)?"
}

step_comment() {
  local before after
  say "4. Comment on a box"
  before="$(diagram_items)"
  echo "  Click a box that has code behind it. The dialog should show its path and lines, the code"
  echo "  with the changed lines tinted, and a Comment field. Write a comment and click Add to draft."
  wait_for "a draft item from a diagram box appeared" 300 test "$(diagram_items)" -gt "$before"
  after="$(diagram_items)"
  ask "Did the dialog close, and does the draft show the comment (on a line, or as a general note naming path:lines)?"
  echo "  Then click a box that is not tied to code (if any): it should say so and put the comment in the review body."
  ask "Does 'Open in diff' go to the file (and light its line when that line is in the change)?"
  echo "  Items from diagram boxes before: $before, now: $after."
}

step_push() {
  say "5. A new commit"
  echo "  Push one small commit to the pull request's branch yourself (any file). The script does not."
  wait_for "the diagrams are reported out of date (stale)" 600 is_stale
  ask "Does the Diagrams footer offer 'Redraw for the new commit', and did nothing redraw on its own?"
}

step_redraw() {
  say "6. Redraw"
  echo "  Click Redraw for the new commit in the window and wait until every entry settles."
  wait_for "version 2 exists" 1800 version_is 2
  wait_for "every entry of version 2 is settled" 1800 all_settled
  ask "Does the footer read 'Version 2 of 2', and does ◀ show version 1 without Redraw or Retry (read-only)?"
  ask "On version 1, does a box open its code, or say 'The code at … is no longer available'?"
}

step_compare() {
  say "7. Compare"
  echo "  On version 2 of a diagram that also exists in version 1, tick 'Show changes since version 1'."
  ask "Are added boxes ringed with a dashed green line, changed ones outlined in amber, and removed ones ghosted and struck through, with the legend's second row?"
  ask "Is the checkbox hidden on version 1 and on a diagram that is new in version 2?"
}

step_cli() {
  say "8. clusia diagrams"
  check "the list prints entry lines and the version line" sh -c 'clusia diagrams "$0" | grep -Eq "^Version [0-9]+ · drawn by"' "$PR"
  check "--json prints the version and the stale flag" sh -c 'clusia diagrams "$0" --json | grep -Eq "\"stale\": ?(true|false)"' "$PR"
  check "a bad option is a usage error (exit 2)" sh -c 'clusia diagrams "$0" --nope >/dev/null 2>&1; test $? -eq 2' "$PR"
}

step_end() {
  say "9. The end"
  echo "  This discards the review of $PR on this Mac (its draft items are lost) and deletes the"
  echo "  drawings. This check does not publish."
  if ! yes_or_no "Discard it now?"; then
    echo "  Skipped: the review was kept, with its draft and drawings. Discard it later with: clusia review discard $PR"
    return
  fi
  if clusia review discard "$PR" >/dev/null 2>"$ERR"; then
    pass "the review was discarded"
  else
    fail "the review was not discarded: $(cat "$ERR")"
  fi
  echo "  Opening it again starts clean: the Diagrams tab offers Draw diagrams (the boxes are put back after this)."
}

summary() {
  printf '\n%d passed, %d failed\n' "$passed" "$failed"
  [ "$failed" -eq 0 ]
}

while [ $# -gt 0 ]; do
  case "$1" in
    --yes) ASSUME_YES=1 ;;
    --list) LIST=1 ;;
    --pr)
      PR="${2:-}"
      [ -n "$PR" ] || {
        usage
        exit 2
      }
      shift
      ;;
    *)
      usage
      exit 2
      ;;
  esac
  shift
done

if [ "$LIST" = 1 ]; then
  printf '%s\n' "${STEPS[@]}"
  exit 0
fi
[ -n "$PR" ] || {
  usage
  exit 2
}
confirm || {
  echo "Nothing was run."
  exit 1
}
step_preflight
step_draw
step_view
step_comment
step_push
step_redraw
step_compare
step_cli
step_end
summary
