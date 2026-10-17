#!/usr/bin/env bash
# Owner check for the 3D keyword view: your own agent, on a pull request of yours (a demo one on
# rzorzal/* is best), with the window in front of you. It spends some of your agent usage (a
# plan turn, one turn per diagram, the two keyword turns and a search of the repository), opens
# a real review of the pull request, and discards that review at the end. It changes five
# Config › Harness settings for the run and puts them back. It publishes nothing to GitHub and
# pushes nothing. Clusia asks you before the agent runs a command, as always, and the script
# never answers for you.
#
#   scripts/sp3-m2-check.sh --pr rzorzal/clusia#123     run the check (asks first)
#   scripts/sp3-m2-check.sh --list                      print the steps and exit
#   add --yes to skip the question below; every step still asks you on the terminal, and
#   without one each of those questions is answered no
#
# Needs Clusia installed (clusia on the PATH, signed in to GitHub) and an agent set up in
# Config › Harness. The window is started for you; look at it when a step says so. If the pull
# request already has a review of yours on this Mac, its draft is lost at the end: use a pull
# request you do not mind opening fresh.
set -u

PR=""
LIST=0
ASSUME_YES=0
passed=0
failed=0
ERR="$(mktemp -t sp3-m2-check.XXXXXX)" || {
  echo "Cannot create a temp file." >&2
  exit 1
}
OLD_ON_OPEN=""
OLD_SECURITY=""
OLD_AUDIT=""
OLD_DIAGRAMS=""
OLD_KEYWORDS=""
CHANGED=0
BEFORE_ITEMS=0
APP_PID=""
DRAW_PID=""
# Set to wait at most this many seconds at every step, instead of each step's own time.
WAIT_CAP="${SP3_CHECK_WAIT:-}"

restore_settings() {
  # The check changes five settings; the owner's own values come back when it ends.
  [ "$CHANGED" = 1 ] || return 0
  [ -n "$OLD_ON_OPEN" ] && clusia config set harness.on_open "$OLD_ON_OPEN" >/dev/null 2>&1
  [ -n "$OLD_SECURITY" ] && clusia config set harness.check_security "$OLD_SECURITY" >/dev/null 2>&1
  [ -n "$OLD_AUDIT" ] && clusia config set harness.audit "$OLD_AUDIT" >/dev/null 2>&1
  [ -n "$OLD_DIAGRAMS" ] && clusia config set harness.diagrams "$OLD_DIAGRAMS" >/dev/null 2>&1
  [ -n "$OLD_KEYWORDS" ] && clusia config set harness.keywords_3d "$OLD_KEYWORDS" >/dev/null 2>&1
  [ -n "$OLD_ON_OPEN" ] || echo "Could not read the old harness.on_open; check Config › Harness: When I open a review." >&2
  [ -n "$OLD_SECURITY" ] || echo "Could not read the old harness.check_security; check Config › Harness: Check security." >&2
  [ -n "$OLD_AUDIT" ] || echo "Could not read the old harness.audit; check Config › Harness: Audit the change." >&2
  [ -n "$OLD_DIAGRAMS" ] || echo "Could not read the old harness.diagrams; check Config › Harness: Draw diagrams." >&2
  [ -n "$OLD_KEYWORDS" ] || echo "Could not read the old harness.keywords_3d; check Config › Harness: 3D keyword view." >&2
  return 0
}
# Stops a drawing the script started, so nothing keeps spending after the check ends.
# A job started in the background of a script may ignore Ctrl-C, so the stop escalates.
stop_draw() {
  local i
  if [ -n "$DRAW_PID" ]; then
    kill -INT "$DRAW_PID" 2>/dev/null
    for i in 1 2 3; do
      kill -0 "$DRAW_PID" 2>/dev/null || break
      sleep 1
    done
    if kill -0 "$DRAW_PID" 2>/dev/null; then
      kill -TERM "$DRAW_PID" 2>/dev/null
      sleep 1
    fi
    if kill -0 "$DRAW_PID" 2>/dev/null; then kill -KILL "$DRAW_PID" 2>/dev/null; fi
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
  "draw: clusia diagrams --draw plans, draws and finds where the words live, and every entry ends ready (or says why not), the 3D one last"
  "view: the Diagrams tab lists Where the words live last with a 3D tag, and its scene shows axes, points, clouds and the legend"
  "usecase: a pill or a cloud name focuses a use case, its card lists the words, empty space deselects, the filters hide points"
  "comment: a click on a point shows its code, and Comment on this line puts a comment from Diagrams into the draft"
  "setting: Turn off in Config opens Config at the 3D keyword view checkbox"
  "cli: clusia diagrams lists the Keywords entry and --json includes the map"
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
  echo "diagrams of the change and the 3D keyword view (two more turns and a search of the"
  echo "repository). You must answer every request for a command yourself: the script never"
  echo "answers for you. It changes five Config › Harness settings for the run and puts them back."
  echo "It discards the review of $PR on this Mac at the end, so a draft you have there is lost."
  echo "It publishes nothing to GitHub."
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
  OLD_KEYWORDS="$(clusia config get harness.keywords_3d 2>/dev/null)"
  echo "  The check turns the four boxes of 'When I open a review' off and the 3D keyword view on,"
  echo "  so the only turns it runs are the ones you see here. Your values (summarize"
  echo "  '$OLD_ON_OPEN', security '$OLD_SECURITY', audit '$OLD_AUDIT', diagrams '$OLD_DIAGRAMS',"
  echo "  3D keyword view '$OLD_KEYWORDS') are put back when it ends."
  yes_or_no "Change those five settings now (they are put back when the check ends)?" || {
    echo "Nothing was changed."
    exit 1
  }
  CHANGED=1
  clusia config set harness.on_open wait >/dev/null 2>&1
  clusia config set harness.check_security false >/dev/null 2>&1
  clusia config set harness.audit false >/dev/null 2>&1
  clusia config set harness.diagrams false >/dev/null 2>&1
  clusia config set harness.keywords_3d true >/dev/null 2>&1
  check "the diagrams box is off" test "$(clusia config get harness.diagrams 2>/dev/null)" = "false"
  check "the 3D keyword view is on" test "$(clusia config get harness.keywords_3d 2>/dev/null)" = "true"
  check "$PR is a pull request clusia can open" clusia open "$PR"
  start_window
}

step_draw() {
  local status=0
  say "2. Draw"
  echo "  This asks your agent for a plan, one drawing per entry and the keyword view (two turns and a"
  echo "  search of the repository). If it asks to run a command, answer in the window: the script"
  echo "  never does."
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
  check "the Keywords entry is listed last and ready" sh -c 'clusia diagrams "$0" | grep -Ev "^(Version |$)" | tail -n 1 | grep -Eq "^keywords .*ready"' "$PR"
}

step_view() {
  say "3. The Diagrams tab"
  echo "  Look at the window and open the Diagrams tab, then select Where the words live."
  ask "Is Where the words live the last entry, with a 3D tag and the line Keywords · N families · M places?"
  ask "Does the scene show three named axes, points as ◆ ● ◉ in family colours with green rings on what the PR changed, faint family lines and named use-case clouds?"
  ask "Do dragging, scrolling, the arrow keys and + − 0 turn and zoom it, and do 1–4 pick 3D and the three 2D views?"
  ask "Does hovering a point light its family and show Matched, What, Where and Use case?"
}

step_usecase() {
  say "4. Use cases and filters"
  ask "Does a pill or a cloud name focus a use case, dim the rest and show its words and The same changes also reach?"
  ask "Does a click on empty space, All use cases or Esc deselect, and a drag never deselect?"
  ask "Do the filters hide families, use cases and unchanged places?"
}

# Whether the draft holds more items from diagram boxes than when the step began. A function,
# so the count is read again on every poll.
items_grew() { [ "$(diagram_items)" -gt "$BEFORE_ITEMS" ]; }

step_comment() {
  local after
  say "5. Comment on a point"
  BEFORE_ITEMS="$(diagram_items)"
  echo "  Click a point in a file the PR changed. The card should show its path and lines and the"
  echo "  code with the line highlighted. Write a comment and click Comment on this line."
  wait_for "a draft item from a diagram box appeared" 300 items_grew
  after="$(diagram_items)"
  ask "Does the card show the code with the line highlighted, and does Comment on this line close it and put the comment in the draft?"
  echo "  Then click a point in a file the PR did not touch and comment on it."
  ask "For a place in a file the PR did not touch, does the card say \"Not in the diff: the comment goes to the review body\" and does the comment arrive as a general note naming path:line?"
  echo "  Items from diagram boxes before: $BEFORE_ITEMS, now: $after."
}

step_setting() {
  say "6. The setting"
  echo "  In the header of the 3D view, click Turn off in Config."
  ask "Does Turn off in Config open Config › Harness with the 3D keyword view row tinted?"
}

step_cli() {
  say "7. clusia diagrams"
  check "the list has the Keywords entry with its families and places" sh -c 'clusia diagrams "$0" | grep -Eq "^keywords .*ready · [0-9]+ famil(y|ies) · [0-9]+ places?"' "$PR"
  check "--json includes the map" sh -c 'clusia diagrams "$0" --json | grep -Eq "ready_map"' "$PR"
  check "a bad option is a usage error (exit 2)" sh -c 'clusia diagrams "$0" --nope >/dev/null 2>&1; test $? -eq 2' "$PR"
}

step_end() {
  say "8. The end"
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
step_usecase
step_comment
step_setting
step_cli
step_end
summary
