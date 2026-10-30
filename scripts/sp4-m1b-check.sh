#!/usr/bin/env bash
# Owner check for the Tests tab and Config › Tests: your own agent and the container engine you
# already have (or none), on a pull request of yours (rzorzal/*), with the window in front of
# you. It refuses every other owner. You do every click yourself: Generate tests, the approvals
# in the setup modal, each run, Stop, Comment on this line, Check again and Clean up…. The script
# only reads what the daemon says (clusia tests, clusia --json tests) and compares it with what
# you see in the window. It spends one agent turn (the one Generate tests asks for, which you
# click), downloads what the setup plan lists once you approve it, runs the pull request's tests
# on this Mac in a container or in Clúsia's sandbox, and discards that review at the end if you
# say so. It changes no setting, installs nothing, starts no engine, approves no step for you,
# removes no Docker object itself and publishes nothing to GitHub.
#
#   scripts/sp4-m1b-check.sh [rzorzal/blog#37]    run the check (asks first)
#   scripts/sp4-m1b-check.sh --list               print the steps and exit
#
# Needs Clúsia installed (clusia on the PATH, signed in to GitHub, the window installed) and an
# agent set up in Config › Harness. If the pull request already has a review of yours on this
# Mac, its draft is lost at the end when you let the script discard it.
set -u

PR=""
OWNER=""
KEY=""
LIST=0
passed=0
failed=0
OLD_RUNNER=""
# Clúsia's data folder, as the CLI finds it.
DATA="${CLUSIA_HOME:-$HOME/Library/Application Support/Clusia}"
ERR="$(mktemp -t sp4-m1b-check.XXXXXX)" || {
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
  "open: the review opens in the window on its Tests tab, which starts empty with Generate tests and runs nothing"
  "generate: click Generate tests; the tab lights its three steps and offers Stop, then the setup plan opens by itself once"
  "approve: read every command in the setup modal and approve it yourself; the host step is amber, the test commands are shown"
  "near: click Run tests near this change; the log streams, the list fills and the Last run card ends with the totals"
  "all: click Run all tests; the results scroll in the list and the window stays responsive"
  "one: click ▶ on one test; only that test runs"
  "stop: click Stop during a run; it ends as Stopped and leaves no container of the review"
  "comment: select a failure and click Comment on this line; the composer opens with the test's name"
  "config: Config › Tests names your engine; Check again answers; Clean up… asks before it removes anything"
  "end: discarding the review leaves no container, stack or test record of it"
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

# Whether the review's JSON view holds `$1` (spaces and newlines removed).
view_has() {
  clusia --json tests "$PR" 2>/dev/null | tr -d ' \n' | grep -q "$1"
}

# No container of this review is left, when this Mac has a docker command to ask.
no_container_left() {
  command -v docker >/dev/null 2>&1 || return 0
  [ -z "$(docker ps -aq --filter "label=io.clusia.review=$KEY" 2>/dev/null)" ]
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
KEY="${BASH_REMATCH[1]}~${BASH_REMATCH[2]}~${BASH_REMATCH[3]}"

if [ "$OWNER" != rzorzal ]; then
  echo "This check runs the pull request's code on your Mac, so it takes only your own" >&2
  echo "pull requests (rzorzal/*), never anyone else's." >&2
  exit 2
fi

echo "This opens a real review of $PR and, when YOU click Generate tests, asks YOUR agent (it"
echo "uses your plan) for a recipe (one turn). Once you approve the setup plan in the window, it"
echo "downloads what the plan lists and runs the pull request's tests on this Mac, in a"
echo "container or in Clúsia's sandbox. You do every click yourself: the script approves nothing,"
echo "runs nothing and stops nothing for you; it only reads what the daemon says and compares it"
echo "with what you see. It changes no setting, installs nothing and starts no engine. It asks"
echo "before it discards the review of $PR on this Mac at the end. It publishes nothing to GitHub."
printf 'Continue? [y/N] '
read -r answer || answer=n
case "$answer" in y | Y | yes) ;; *) exit 1 ;; esac

say "${STEPS[0]}"
command -v clusia >/dev/null 2>&1 || { fail "clusia is on the PATH"; exit 1; }
check "the daemon answers" clusia daemon status
check "the review opens" clusia open "$PR"
OLD_RUNNER="$(clusia config get tests.runner 2>/dev/null)"
clusia tests "$PR"
ask "The Tests tab is empty, offers Generate tests and shows nothing running"

say "${STEPS[1]}"
go_on "click Generate tests in the window (one agent turn on your account)"
pause "wait for the setup plan to open by itself, or for a message if it failed"
clusia tests "$PR"
check "the daemon agrees the recipe needs your approval" view_has '"phase":"needs_setup"'
ask "The modal opened once on its own, and closing it did not open it again"

say "${STEPS[2]}"
echo "  Read each command and where it runs; the amber row is the one that runs on your Mac."
echo "  Approve it yourself with Approve selected or Approve all and set up."
pause "wait for the setup to finish"
clusia tests "$PR"
check "the daemon says the setup is done" view_has '"phase":"ready"'
ask "Every step you approved shows as done, and nothing you did not approve ran"

say "${STEPS[3]}"
go_on "click Run tests near this change"
pause "wait for the run to end"
clusia tests "$PR"
check "the daemon holds a run" view_has '"runs":\[{'
ask "The last line of this terminal and the Last run card agree (passed, failed, skipped, time, where)"

say "${STEPS[4]}"
go_on "click Run all tests"
pause "wait for the run to end"
ask "The list kept scrolling and the window answered clicks while the results came in"

say "${STEPS[5]}"
go_on "click ▶ on one test in the list"
pause "wait for the run to end"
ask "Only that one test ran, and the Last run card says one test"

say "${STEPS[6]}"
go_on "start a run and click Stop while it goes"
pause "wait for the run to end"
check "the daemon records the run as stopped" view_has '"outcome":"stopped"'
sleep 2
check "no container of the review is left" no_container_left
ask "The tab shows Stopped, not a failure"

say "${STEPS[7]}"
echo "  Select a failed test, then click Comment on this line. Nothing is published: the"
echo "  comment goes to the draft of this review, which is discarded at the end."
ask "The composer (or the general note) opened with the test's name and its place"

say "${STEPS[8]}"
echo "  Open Config › Tests. Click Check again. Click Clean up… and then Cancel."
ask "The engine line names what you have, and Check again answered"
ask "Clean up… asked before it did anything, and Cancel left everything as it was"
[ "$(clusia config get tests.runner 2>/dev/null)" = "$OLD_RUNNER" ] \
  && pass "tests.runner is as it was" || fail "tests.runner is as it was"

say "${STEPS[9]}"
if yes_or_no "Discard the review of $PR on this Mac now"; then
  check "the review is discarded" clusia review discard "$PR"
  sleep 2
  check "no container of the review is left" no_container_left
  check "the test record is gone" test ! -e "$DATA/tests/$KEY.json"
else
  echo "  Skipped: the review was kept."
fi

printf '\n%d passed, %d failed\n' "$passed" "$failed"
[ "$failed" = 0 ]
