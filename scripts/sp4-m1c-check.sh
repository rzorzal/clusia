#!/usr/bin/env bash
# Owner check for Run Function tests: your own agent and the container engine you already have
# (or none), on a pull request of yours (rzorzal/*), with the window in front of you. It
# refuses every other owner. You do every click yourself: Generate tests, the approvals in the
# setup modal, editing a value, each run, Stop, Show driver code and Comment on the function.
# The script approves nothing and runs nothing but the two `clusia tests` reads and the one
# `clusia tests --run-cases` it asks before. It spends one agent turn for Generate tests and one
# more for each New fake values or Ask … for more cases you click, and the cases run drivers of
# the pull request on this Mac, in the container or the sandbox you approved. It changes no
# setting it does not put back, installs nothing, starts no engine, removes no Docker object
# itself and publishes nothing to GitHub. It discards the review only when you say yes.
#
#   scripts/sp4-m1c-check.sh [rzorzal/blog#37]    run the check (asks first)
#   scripts/sp4-m1c-check.sh --list               print the steps and exit
#
# Needs Clúsia installed (clusia on the PATH, signed in to GitHub, the window installed) and an
# agent set up in Config › Harness. If the pull request already has a review of yours on this
# Mac, its draft is lost at the end when you let the script discard it.
set -u

PR=""
OWNER=""
REPO=""
NUMBER=""
KEY=""
LIST=0
SKIP_CASES=0
passed=0
failed=0
OLD_RUNNER=""
# Clúsia's data folder, as the CLI finds it.
DATA="${CLUSIA_HOME:-$HOME/Library/Application Support/Clusia}"
ERR="$(mktemp -t sp4-m1c-check.XXXXXX)" || {
  echo "Cannot create a temp file." >&2
  exit 1
}

drop_err() {
  if [ -n "$ERR" ]; then rm -f "$ERR" "$ERR".*; fi
}
trap 'drop_err' EXIT
# clusia exits 130 on a Ctrl-C by status, not by the signal; the script ends instead.
trap 'exit 130' INT TERM HUP

STEPS=(
  "open: the review opens in the window on its Tests tab"
  "generate: click Generate tests; one agent turn writes the recipe and the cases for the changed functions"
  "approve: the setup plan lists the function commands next to the test commands; approve it yourself"
  "functions: open Run Function tests; the changed functions are listed, nothing has run"
  "edit: change a value and press Enter; the field keeps it and the case's Reset button appears"
  "run: click Run this case; the result shows what the call returned, the state after it and the output"
  "fake: click New fake values; the agent writes new values for that case"
  "reset: click Reset; the agent's values come back"
  "add: click + Add a case, then Remove it"
  "more: click Ask your agent for more cases; new cases are appended"
  "all: click Run all cases, then click Stop during one run; the case that runs ends Stopped and the rest do not run"
  "driver: click Show driver code; the driver opens as plain text"
  "comment: click Comment on the function; the composer opens with the function and its failing case"
  "cli: clusia tests <pr> --functions and --run-cases give the same cases and results as the window"
  "end: discarding the review leaves no cases, no placed driver and no container of it"
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

# Waits for you to finish what the window needs (a click, a modal, a run).
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

# Whether the review's tests view holds `$1` (spaces and newlines removed).
tests_has() {
  clusia --json tests "$PR" 2>/dev/null | tr -d ' \n' | grep -q "$1"
}

# Whether the review's function view holds `$1` (spaces and newlines removed).
view_has() {
  clusia --json tests "$PR" --functions 2>/dev/null | tr -d ' \n' | grep -q "$1"
}

# Whether a changed function of the pull request is in a language Clúsia runs.
has_supported() {
  view_has '"lang":"'
}

# No container of this review is left, when this Mac has a docker command to ask.
no_container_left() {
  command -v docker >/dev/null 2>&1 || return 0
  [ -z "$(docker ps -aq --filter "label=io.clusia.review=$KEY" 2>/dev/null)" ]
}

# No driver Clúsia placed in the review's worktree is left (a Rust driver is a file under
# tests/ for the length of a run); a worktree that is gone has none.
no_placed_driver() {
  local wt="$DATA/worktrees/${OWNER}~${REPO}~${NUMBER}"
  [ -d "$wt" ] || return 0
  ! git -C "$wt" status --porcelain 2>/dev/null | grep -q clusia_driver_
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

echo "This opens a real review of $PR and, when YOU click Generate tests, asks YOUR agent (it"
echo "uses your plan) for a recipe and for cases with fake values for the changed functions (one"
echo "turn), then one more turn for each New fake values or Ask … for more cases you click. The"
echo "cases run drivers of the pull request on this Mac, in the container or the sandbox you"
echo "approved in the setup plan. You click everything: the script approves nothing and runs"
echo "nothing but the two clusia tests reads and the one --run-cases it asks before. It puts"
echo "back any setting it reads (tests.runner), installs nothing and starts no engine. It asks"
echo "before it discards the review of $PR on this Mac at the end. It publishes nothing to GitHub."
printf 'Continue? [y/N] '
read -r answer || answer=n
case "$answer" in y | Y | yes) ;; *) exit 1 ;; esac

say "${STEPS[0]}"
command -v clusia >/dev/null 2>&1 || { fail "clusia is on the PATH"; exit 1; }
check "the daemon answers" clusia daemon status
check "the review opens" clusia open "$PR"
OLD_RUNNER="$(clusia config get tests.runner 2>/dev/null)"
ask "The review opened on its Tests tab"

say "${STEPS[1]}"
go_on "click Generate tests in the window (one agent turn on your account)"
pause "wait for the setup plan to open by itself, or for a message if it failed"
check "the daemon agrees the recipe needs your approval" tests_has '"phase":"needs_setup"'

say "${STEPS[2]}"
echo "  Read each command and where it runs: the function commands are listed with the test"
echo "  commands. Approve it yourself with Approve selected or Approve all and set up."
pause "wait for the setup to finish"
check "the daemon says the setup is done" tests_has '"phase":"ready"'
ask "Every step you approved shows as done, and nothing you did not approve ran"

say "${STEPS[3]}"
echo "  Click the Run Function tests sub-tab."
clusia tests "$PR" --functions
check "the daemon lists the changed functions" view_has '"functions":\[{'
ask "The window lists the same functions, and no case has a result yet"
if ! has_supported; then
  echo "No changed function of this pull request is in a language Clúsia runs; skipping the case steps."
  SKIP_CASES=1
fi

if [ "$SKIP_CASES" = 0 ]; then
  say "${STEPS[4]}"
  echo "  Change a value in a case and press Enter."
  ask "The field kept the value and the case's Reset button appeared"

  say "${STEPS[5]}"
  go_on "click Run this case"
  pause "wait for the case to end"
  check "the daemon holds a result" view_has '"status":"'
  ask "The result shows what the call returned, the state after the call and the output"

  say "${STEPS[6]}"
  go_on "click New fake values (one agent turn on your account)"
  pause "wait for the new values"
  ask "The case holds new values written by the agent"

  say "${STEPS[7]}"
  echo "  Click Reset on that case."
  ask "The agent's values came back and Reset went away"

  say "${STEPS[8]}"
  echo "  Click + Add a case, then click Remove on it."
  ask "A case appeared with empty values, and Remove took it away"

  say "${STEPS[9]}"
  go_on "click Ask your agent for more cases (one agent turn on your account)"
  pause "wait for the cases"
  ask "New cases were appended and the existing ones kept their values"

  say "${STEPS[10]}"
  go_on "click Run all cases, then click Stop while one case runs"
  pause "wait for the run to end"
  sleep 2
  check "no container of the review is left" no_container_left
  check "no driver is left in the worktree" no_placed_driver
  ask "The case that ran ended Stopped, and the cases after it did not run"

  say "${STEPS[11]}"
  echo "  Click Show driver code on a result."
  ask "The driver opened as plain text, with Open in editor and Close"

  say "${STEPS[12]}"
  echo "  Click Comment on the function. Nothing is published: the comment goes to the draft of"
  echo "  this review, which is discarded at the end."
  ask "The composer opened with the function and its failing case"
fi

say "${STEPS[13]}"
clusia tests "$PR" --functions
if [ "$SKIP_CASES" = 0 ]; then
  go_on "run clusia tests $PR --run-cases (every case of the pull request, in the container or sandbox you approved)"
  clusia tests "$PR" --run-cases
  code=$?
  echo "  clusia exited $code (0 every case passed, 1 any failed or erred, 124 timed out, 130 stopped)."
  ask "That exit code matches the window (0 passed, 1 failed or erred, 124 timed out, 130 stopped)"
  check "the daemon holds results" view_has '"status":"'
fi
ask "The cases listed above are the ones the window shows"

say "${STEPS[14]}"
if yes_or_no "Discard the review of $PR on this Mac now"; then
  check "the review is discarded" clusia review discard "$PR"
  sleep 2
  check "no container of the review is left" no_container_left
  check "the test record is gone" test ! -e "$DATA/tests/$KEY.json"
  check "no driver is left in the worktree" no_placed_driver
else
  echo "  Skipped: the review was kept."
fi
if [ "$(clusia config get tests.runner 2>/dev/null)" = "$OLD_RUNNER" ]; then
  pass "tests.runner is as it was"
else
  fail "tests.runner is as it was"
fi

printf '\n%d passed, %d failed\n' "$passed" "$failed"
[ "$failed" = 0 ]
