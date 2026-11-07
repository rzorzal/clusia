#!/usr/bin/env bash
# Owner check for the QA cockpit's screen: it opens the window on demo data for six scenes, then
# on your own pull request (rzorzal/*); it refuses every other owner. Your own agent spends one
# turn for each Write scenarios, Write it, Ask for new values and copilot message you click, and
# the runs execute the pull request's code on this Mac, in the container or sandbox you approved.
# You click everything: the script approves nothing and runs nothing but the two reads it makes
# of `clusia tests <pr> --scenarios --json`. It changes no setting, publishes nothing to GitHub
# and discards the review only when you say yes.
#
#   scripts/sp4-m2b-check.sh [rzorzal/blog#37]    run the check (asks first)
#   scripts/sp4-m2b-check.sh --list               print the steps and exit
#
# Needs Clúsia installed (clusia and clusia-app on the PATH, or CLUSIA_APP set to the window's
# binary), signed in to GitHub, an agent set up in Config › Harness, and a review whose tests are
# already set up. If the pull request already has a review of yours on this Mac, its draft is
# lost at the end when you let the script discard it.
set -u

PR=""
OWNER=""
REPO=""
NUMBER=""
KEY=""
LIST=0
passed=0
failed=0
APP_PID=""
# Clúsia's data folder, as the CLI finds it.
DATA="${CLUSIA_HOME:-$HOME/Library/Application Support/Clusia}"
ERR="$(mktemp -t sp4-m2b-check.XXXXXX)" || {
  echo "Cannot create a temp file." >&2
  exit 1
}

# Ends the demo window the script started, and removes the temp file.
cleanup() {
  if [ -n "$APP_PID" ]; then kill "$APP_PID" 2>/dev/null; fi
  if [ -n "$ERR" ]; then rm -f "$ERR" "$ERR".*; fi
}
trap 'cleanup' EXIT
trap 'exit 130' INT TERM HUP

STEPS=(
  "demo-entry: the window opens on cockpit-entry; the sub-tab bar ends with Open the QA cockpit →"
  "demo-empty: cockpit-empty shows the empty card and 'Nothing runs until you click Run'"
  "demo-scenario: cockpit-scenario shows the three-column screen, the chart with a solid and a dashed line, the table and the verdict line"
  "demo-bench: cockpit-bench shows 'live', Stop and no verdict"
  "demo-approve: cockpit-approve shows the command, the script and Cancel / Approve / Approve and run"
  "demo-main: cockpit-main shows the setup steps and Approve and prepare"
  "open: your real review opens in the window on its Tests tab; the daemon answers"
  "cockpit: click Open the QA cockpit →; the list matches clusia --json tests PR --scenarios (the script compares the counts)"
  "write: click Write scenarios (or + Ask the copilot for a scenario, then Write it); one agent turn; scenarios appear unapproved, nothing ran"
  "edit: change an input and press Enter; the field keeps it with the green border and Reset appears; a bad value shows the error under the field and sends nothing"
  "approve: click Approve and run…; read the command and the script; Approve; the scenario says approved"
  "run: Run on this PR; the chart fills live; the result and Errors show; the exit state matches --scenarios"
  "main: tick Run on main too; read Prepare main; approve it yourself; run both; the table has a Δ and a verdict, or ≈ inside the spread"
  "stop: start a comparison and click ■ Stop; the finished rounds stay and the verdict says no verdict"
  "copilot: ask the copilot something; Explain this result; a suggestion's Write it writes one item; nothing runs by itself"
  "comment: Suggest as a test in a comment; the general comment opens with the text; you post or discard it yourself"
  "end: discarding the review leaves no main worktree, no placed bench file and no container or volume of it"
)

say() { printf '\n%s\n' "$1"; }
pass() { passed=$((passed + 1)); printf '  PASS  %s\n' "$1"; }
fail() { failed=$((failed + 1)); printf '  FAIL  %s\n' "$1"; }

check() {
  local what="$1"
  shift
  if "$@" >/dev/null 2>"$ERR"; then pass "$what"; else fail "$what"; sed 's/^/        /' "$ERR"; fi
}

yes_or_no() {
  local answer
  read -r -p "  ? $1 [y/n] " answer || answer=n
  case "$answer" in y | Y | yes) return 0 ;; *) return 1 ;; esac
}

ask() {
  if yes_or_no "$1"; then pass "$1"; else fail "$1"; fi
}

# Waits for you to finish what the step needs.
pause() {
  local ignored
  read -r -p "  ... $1 (press Enter when done) " ignored || true
}

# Asks before a step that spends or changes something; a no ends the check there.
go_on() {
  local answer
  printf '  Next: %s\n  Continue? [y/N] ' "$1"
  read -r answer || answer=n
  case "$answer" in
    y | Y | yes) return 0 ;;
    *)
      echo "  Stopped before: $1"
      exit 1
      ;;
  esac
}

usage() {
  echo "usage: $0 [--list | rzorzal/repo#N]" >&2
}

# Whether the cockpit view holds `$1` (spaces and newlines removed).
cockpit_has() {
  clusia --json tests "$PR" --scenarios 2>/dev/null | tr -d ' \n' | grep -q "$1"
}

no_main_worktree() {
  [ ! -e "$DATA/worktrees/.main/$KEY" ]
}

# No Docker volume of this review is left, when this Mac has a docker command to ask.
no_main_volume() {
  command -v docker >/dev/null 2>&1 || return 0
  [ -z "$(docker volume ls -q --filter "label=io.clusia.review=$KEY" 2>/dev/null)" ]
}

# No container of this review is left, when this Mac has a docker command to ask.
no_container_left() {
  command -v docker >/dev/null 2>&1 || return 0
  [ -z "$(docker ps -aq --filter "label=io.clusia.review=$KEY" 2>/dev/null)" ]
}

# No bench file Clúsia placed in the review's worktree is left (a Rust benchmark is a file under
# tests/ for the length of a run); a worktree that is gone has none.
no_placed_bench() {
  local wt="$DATA/worktrees/$KEY"
  [ -d "$wt" ] || return 0
  ! git -C "$wt" status --porcelain 2>/dev/null | grep -q clusia_bench_
}

# Opens the window on one demo scene, and ends it when you press Enter.
demo_scene() {
  "${CLUSIA_APP:-clusia-app}" --demo --scene "$1" >/dev/null 2>&1 &
  APP_PID=$!
  sleep 2
  pause "look at the window, then close this scene"
  kill "$APP_PID" 2>/dev/null
  wait "$APP_PID" 2>/dev/null
  APP_PID=""
}

# How many scenarios and benchmarks the daemon lists for the review, as "S B".
counts() {
  clusia --json tests "$PR" --scenarios 2>/dev/null | python3 -I -c '
import json, sys
v = json.load(sys.stdin)
print(len(v.get("scenarios", [])), len(v.get("benches", [])))'
}

while [ $# -gt 0 ]; do
  case "$1" in
    --list) LIST=1 ;;
    -*) usage; exit 2 ;;
    *) [ -z "$PR" ] || { usage; exit 2; }; PR="$1" ;;
  esac
  shift
done

if [ "$LIST" = 1 ]; then
  printf '%s\n' "${STEPS[@]}"
  exit 0
fi
[ -n "$PR" ] || PR="rzorzal/blog#37"
if ! [[ "$PR" =~ ^([A-Za-z0-9-]+)/([A-Za-z0-9._-]+)#([0-9]+)$ ]]; then
  usage
  exit 2
fi
OWNER="${BASH_REMATCH[1]}"
REPO="${BASH_REMATCH[2]}"
NUMBER="${BASH_REMATCH[3]}"
KEY="${OWNER}~${REPO}~${NUMBER}"

if [ "$OWNER" != rzorzal ]; then
  echo "This check runs the pull request's code on your Mac, so it takes only your own" >&2
  echo "pull requests (rzorzal/*), never anyone else's." >&2
  exit 2
fi

echo "This opens the Clúsia window on demo data for six scenes, then on your own pull request $PR."
echo "Your own agent spends one turn for each Write scenarios, Write it, Ask for new values and"
echo "copilot message you click. Runs execute the pull request's code on this Mac, in the container"
echo "or the sandbox you approved. You click everything: the script approves nothing and runs"
echo "nothing but the two reads it makes of 'clusia tests $PR --scenarios --json'. It changes no"
echo "setting, asks before it discards the review of $PR on this Mac at the end, and publishes"
echo "nothing to GitHub."
printf 'Continue? [y/N] '
read -r answer || answer=n
case "$answer" in y | Y | yes) ;; *) exit 1 ;; esac

DEMOS=(cockpit-entry cockpit-empty cockpit-scenario cockpit-bench cockpit-approve cockpit-main)
for i in 0 1 2 3 4 5; do
  say "${STEPS[$i]}"
  go_on "open the window on ${DEMOS[$i]} (demo data, nothing is sent)"
  demo_scene "${DEMOS[$i]}"
  ask "You saw what the step says"
done

say "${STEPS[6]}"
command -v clusia >/dev/null 2>&1 || { fail "clusia is on the PATH"; exit 1; }
check "the daemon answers" clusia daemon status
go_on "open $PR in the window (a real review on this Mac)"
check "the review opens" clusia open "$PR"
pause "wait until the review is ready and open its Tests tab"

say "${STEPS[7]}"
pause "click Open the QA cockpit →"
read -r SCENARIOS BENCHES < <(counts) || true
echo "  The daemon lists ${SCENARIOS:-?} scenarios and ${BENCHES:-?} benchmarks."
ask "The left list shows the same ${SCENARIOS:-?} scenarios and ${BENCHES:-?} benchmarks"

say "${STEPS[8]}"
go_on "click Write scenarios in the window (one agent turn on your account)"
pause "click Write scenarios (or + Ask the copilot for a scenario, then Write it) and wait for the answer"
read -r SCENARIOS BENCHES < <(counts) || true
if [ "${SCENARIOS:-0}" -gt 0 ] 2>/dev/null; then
  pass "the daemon lists $SCENARIOS scenario(s)"
  check "none is approved yet" cockpit_has '"approval":"not_approved"'
  ask "Scenarios appeared in the window unapproved and nothing ran"
else
  say "No scenario was written, so the steps that need one are skipped."
  SCENARIOS=0
fi

if [ "$SCENARIOS" -gt 0 ]; then
  say "${STEPS[9]}"
  pause "change an input and press Enter; then type a bad value and press Enter"
  ask "The field kept the value with a green border, Reset appeared, and the bad value showed its error and sent nothing"

  say "${STEPS[10]}"
  pause "click Approve and run…, read the command and the script, and click Approve"
  check "the scenario says approved" cockpit_has '"approval":"approved"'

  say "${STEPS[11]}"
  go_on "click Run on this PR (runs the pull request's code on this Mac)"
  pause "wait until the run ends"
  ask "The chart filled live, and the result and Errors match what clusia tests $PR --scenarios says"

  say "${STEPS[12]}"
  go_on "tick Run on main too, approve Prepare main yourself and run both (main's worktree takes disk of its own)"
  pause "wait until both sides finished"
  ask "The table has a Δ and a verdict, or ≈ inside the spread"

  say "${STEPS[13]}"
  go_on "start a comparison and click ■ Stop while a round runs"
  pause "click Stop and wait for the window to settle"
  ask "The finished rounds stayed and the verdict says no verdict"

  say "${STEPS[14]}"
  go_on "ask the copilot something and click Explain this result (agent turns on your account)"
  pause "ask, explain, and click Write it on one suggestion"
  ask "Each answer was plain text, a suggestion's Write it wrote one item, and nothing ran by itself"

  say "${STEPS[15]}"
  pause "click Suggest as a test in a comment"
  ask "The general comment opened with the text and nothing was posted"
fi

say "${STEPS[16]}"
if yes_or_no "Discard the review of $PR on this Mac now"; then
  check "the review is discarded" clusia review discard "$PR"
  sleep 2
  check "no main worktree is left" no_main_worktree
  check "no volume of the review is left" no_main_volume
  check "no container of the review is left" no_container_left
  check "no bench file is left in the worktree" no_placed_bench
else
  echo "  Skipped: the review was kept."
fi

printf '\n%d passed, %d failed\n' "$passed" "$failed"
[ "$failed" = 0 ]
