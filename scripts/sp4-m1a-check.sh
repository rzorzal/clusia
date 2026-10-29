#!/usr/bin/env bash
# Owner check for the tests of a review: your own agent and the container engine you
# already have (or none), on a pull request of yours (rzorzal/*), with this terminal in front
# of you. It refuses every other owner. It spends one agent turn (the recipe), downloads what the
# setup plan lists once you approve it, runs the pull request's tests on this Mac in a container
# or in Clúsia's sandbox, and discards that review at the end if you say so. It changes
# tests.runner for one step and puts it back. It installs nothing, starts no engine, approves no
# step for you, removes no Docker object itself and publishes nothing to GitHub.
#
#   scripts/sp4-m1a-check.sh rzorzal/clusia#123    run the check (asks first)
#   scripts/sp4-m1a-check.sh --list                print the steps and exit
#
# Before each step that spends or changes something (generating the recipe, each run, the
# runner switch, the discard) it asks again, and a no ends the check there.
#
# Needs Clusia installed (clusia on the PATH, signed in to GitHub) and an agent set up in
# Config › Harness. If the pull request already has a review of yours on this Mac, its draft is
# lost at the end when you let the script discard it.
set -u

PR=""
OWNER=""
KEY=""
LIST=0
passed=0
failed=0
OLD_RUNNER=""
CHANGED=0
RUN_PID=""
FIRST_TEST=""
# Clúsia's data folder, as the CLI finds it.
DATA="${CLUSIA_HOME:-$HOME/Library/Application Support/Clusia}"
ERR="$(mktemp -t sp4-m1a-check.XXXXXX)" || {
  echo "Cannot create a temp file." >&2
  exit 1
}

restore_settings() {
  [ "$CHANGED" = 1 ] || return 0
  if [ -n "$OLD_RUNNER" ]; then
    clusia config set tests.runner "$OLD_RUNNER" >/dev/null 2>&1
  else
    echo "Could not read the old tests.runner; check Config › Tests: Where tests run." >&2
  fi
  return 0
}
# Stops a run the script started, so nothing keeps running after the check ends.
stop_run() {
  local i
  if [ -n "$RUN_PID" ]; then
    kill -INT "$RUN_PID" 2>/dev/null
    for i in 1 2 3 4 5; do
      kill -0 "$RUN_PID" 2>/dev/null || break
      sleep 1
    done
    if kill -0 "$RUN_PID" 2>/dev/null; then kill -KILL "$RUN_PID" 2>/dev/null; fi
    wait "$RUN_PID" 2>/dev/null
    RUN_PID=""
  fi
}
drop_err() {
  if [ -n "$ERR" ]; then rm -f "$ERR" "$ERR".*; fi
}
trap 'stop_run; restore_settings; drop_err' EXIT
# clusia exits 130 on a Ctrl-C by status, not by the signal; the script ends instead.
trap 'exit 130' INT TERM HUP

STEPS=(
  "preflight: clusia is installed, the daemon answers and the review opens; the Tests state is printed"
  "generate: clusia tests --generate probes the machine, names your engine (or the local sandbox) and writes the recipe"
  "unapproved: a run before the setup is approved is refused and runs nothing"
  "setup: clusia tests --setup shows the plan with each command and where it runs, and asks you"
  "list: clusia tests --list prints the tests the recipe finds"
  "near: clusia tests --run --near runs the tests near the change and ends with the totals"
  "one: clusia tests --run --one runs a single test"
  "stop: a Ctrl-C during clusia tests --run --all stops it with exit 130 and leaves no container"
  "local: with tests.runner = local the same run goes through Clúsia's sandbox"
  "cli: clusia --json tests prints the view, and a flag without its partner is a usage error"
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

echo "This opens a real review of $PR and asks YOUR agent (it uses your plan) for a recipe"
echo "(one turn). Once you approve the setup plan, it downloads what the plan lists and"
echo "runs the pull request's tests on this Mac, in a container or in Clúsia's sandbox. You"
echo "answer the setup question yourself: the script never approves for you. It installs nothing"
echo "and starts no engine. It sets tests.runner to local for one step and puts it back. It asks"
echo "before it discards the review of $PR on this Mac at the end. It publishes nothing to GitHub."
printf 'Continue? [y/N] '
read -r answer || answer=n
case "$answer" in y | Y | yes) ;; *) exit 1 ;; esac

say "${STEPS[0]}"
command -v clusia >/dev/null 2>&1 || { fail "clusia is on the PATH"; exit 1; }
check "the daemon answers" clusia daemon status
check "the review opens" clusia open "$PR"
clusia tests "$PR"
OLD_RUNNER="$(clusia config get tests.runner 2>/dev/null)"

say "${STEPS[1]}"
go_on "your agent writes the recipe (one turn)"
check "clusia tests --generate ends" clusia tests "$PR" --generate
clusia tests "$PR"
ask "The first line names your engine and its state (or says local sandbox) correctly"

say "${STEPS[2]}"
go_on "a run that must be refused, since nothing is approved yet"
if clusia tests "$PR" --run --near >"$ERR.out" 2>"$ERR"; then
  fail "a run before the setup is refused"
else
  grep -q "Set up" "$ERR" && pass "a run before the setup is refused" || fail "a run before the setup is refused"
fi

say "${STEPS[3]}"
echo "  Read each command and where it runs, then answer yourself."
if clusia tests "$PR" --setup </dev/tty; then pass "the setup finished"; else fail "the setup finished"; fi

say "${STEPS[4]}"
if clusia tests "$PR" --list >"$ERR.out" 2>"$ERR" && [ -s "$ERR.out" ]; then
  pass "the tests are listed"
  FIRST_TEST="$(head -n 1 "$ERR.out" | sed 's/  .*$//')"
else
  fail "the tests are listed"
fi

say "${STEPS[5]}"
go_on "run the tests near the change"
clusia tests "$PR" --run --near
ask "The results and the last line (passed, failed, skipped, the time, where it ran) look right"

say "${STEPS[6]}"
if [ -n "$FIRST_TEST" ]; then
  go_on "run $FIRST_TEST alone"
  clusia tests "$PR" --run --one "$FIRST_TEST"
  ask "Only $FIRST_TEST ran"
else
  fail "a test to run alone (the list was empty)"
fi

say "${STEPS[7]}"
go_on "run all the tests and stop them after 10 seconds"
clusia tests "$PR" --run --all >"$ERR.out" 2>"$ERR" &
RUN_PID=$!
sleep 10
kill -INT "$RUN_PID" 2>/dev/null
wait "$RUN_PID"
status=$?
RUN_PID=""
[ "$status" = 130 ] && pass "the stopped run exits 130" || fail "the stopped run exits 130 (got $status)"
sleep 2
check "no container of the review is left" no_container_left

say "${STEPS[8]}"
go_on "set tests.runner to local, set up and run there (it is put back after)"
CHANGED=1
clusia config set tests.runner local >/dev/null
echo "  The plan changes with the runner; answer the setup question yourself."
clusia tests "$PR" --setup </dev/tty
clusia tests "$PR" --run --near
ask "The last line says local sandbox"
restore_settings
CHANGED=0

say "${STEPS[9]}"
check "the view prints as JSON" sh -c "clusia --json tests '$PR' | grep -q '\"view\"'"
if clusia tests "$PR" --near >/dev/null 2>&1; then
  fail "--near without --run is a usage error"
else
  [ $? = 2 ] && pass "--near without --run is a usage error" || fail "--near without --run is a usage error"
fi

say "${STEPS[10]}"
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
