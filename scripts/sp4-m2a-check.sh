#!/usr/bin/env bash
# Owner check for the QA cockpit's engine: your own agent and the container engine you already
# have (or none), on a pull request of yours (rzorzal/*). It refuses every other owner. The
# script drives `clusia tests` itself, because the window for the cockpit comes in the next
# milestone. It spends one agent turn for each of `--write-scenarios`, `--bench … --write` and
# `--copilot`, and it runs the pull request's code on this Mac, in the container or sandbox you
# approved, for each run. It approves nothing for you (each approval is a question you answer at
# the terminal), changes no setting it does not put back, installs nothing, starts no engine,
# removes no Docker object itself and publishes nothing to GitHub. It discards the review only
# when you say yes.
#
#   scripts/sp4-m2a-check.sh [rzorzal/blog#37]    run the check (asks first)
#   scripts/sp4-m2a-check.sh --list               print the steps and exit
#
# Needs Clúsia installed (clusia on the PATH, signed in to GitHub) and an agent set up in
# Config › Harness, and a review whose tests are already set up (clusia tests <pr> --generate
# and --setup). If the pull request already has a review of yours on this Mac, its draft is
# lost at the end when you let the script discard it.
set -u

PR=""
OWNER=""
REPO=""
NUMBER=""
KEY=""
LIST=0
SLUG=""
passed=0
failed=0
OLD_RUNNER=""
# Clúsia's data folder, as the CLI finds it.
DATA="${CLUSIA_HOME:-$HOME/Library/Application Support/Clusia}"
ERR="$(mktemp -t sp4-m2a-check.XXXXXX)" || {
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
  "open: the review opens and the cockpit is empty (--scenarios lists none)"
  "write: clusia tests --write-scenarios; one agent turn writes scenarios from the diff"
  "list: --scenarios lists them, none approved, nothing run"
  "approve: --approve-scenario SLUG prints the command and every script; you read them and answer"
  "edit-guard: change a script in .clusia/e2e/<slug>/ by hand; the scenario shows 'approve again: the script changed'"
  "run: --run-scenarios --scenario SLUG runs the PR side; the rounds stream and the exit code matches"
  "main: --run-scenarios --on-main prints main's own plan; you approve it; main's worktree appears under worktrees/.main"
  "compare: the table has medians and a Δ, and a verdict line; a gap inside the spread reads ≈"
  "stop: Ctrl-C during a comparison; the exit code is 130, the finished rounds are kept and the verdict is 'Stopped: no verdict'"
  "bench: --bench NAME --write, then --bench NAME --on-main; p50, p95 and calls/s appear for both sides"
  "copilot: --copilot explain --scenario SLUG prints plain text; --copilot 'what next?' prints suggestions as text"
  "comment: --as-comment SLUG prints a suggested comment; nothing is posted"
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

# The first scenario's slug.
first_slug() {
  clusia --json tests "$PR" --scenarios 2>/dev/null | tr -d ' \n' \
    | sed -n 's/.*"slug":"\([a-z0-9-]*\)".*/\1/p' | head -1
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

echo "This opens a real review of $PR and drives clusia tests: one agent turn of YOUR agent for"
echo "each of --write-scenarios, --bench … --write and --copilot, and runs of the pull request's"
echo "code on this Mac, in the container or the sandbox you approved. Every approval is a"
echo "question you answer at the terminal; the script approves nothing. It puts back any setting"
echo "it reads (tests.runner), installs nothing and starts no engine. It asks before it discards"
echo "the review of $PR on this Mac at the end. It publishes nothing to GitHub."
printf 'Continue? [y/N] '
read -r answer || answer=n
case "$answer" in y | Y | yes) ;; *) exit 1 ;; esac

say "${STEPS[0]}"
command -v clusia >/dev/null 2>&1 || { fail "clusia is on the PATH"; exit 1; }
check "the daemon answers" clusia daemon status
check "the review opens" clusia open "$PR"
OLD_RUNNER="$(clusia config get tests.runner 2>/dev/null)"
clusia tests "$PR" --scenarios
check "the cockpit answers" cockpit_has '"scenarios":\['

say "${STEPS[1]}"
go_on "run clusia tests $PR --write-scenarios (one agent turn on your account)"
clusia tests "$PR" --write-scenarios
SLUG="$(first_slug)"
check "a scenario was written" test -n "$SLUG"

say "${STEPS[2]}"
clusia tests "$PR" --scenarios
check "none is approved" cockpit_has '"approval":"not_approved"'
ask "Nothing ran, and every scenario says it is not approved"

say "${STEPS[3]}"
echo "  Read the command and every script, then answer the question."
clusia tests "$PR" --approve-scenario "$SLUG"
check "the scenario is approved" cockpit_has '"approval":"approved"'

say "${STEPS[4]}"
SCRIPT="$(find "$DATA/worktrees/$KEY/.clusia/e2e/$SLUG" -maxdepth 1 -type f ! -name '*.toml' ! -name 'edits.json' 2>/dev/null | head -1)"
if [ -n "$SCRIPT" ]; then
  printf '\n# edited by the check\n' >>"$SCRIPT"
  check "the scenario asks to be approved again" \
    bash -c "clusia tests '$PR' --scenarios | grep -q 'approve again: the script changed'"
  echo "  Approve it again (read what changed)."
  clusia tests "$PR" --approve-scenario "$SLUG"
else
  fail "a script of the scenario was found under .clusia/e2e/$SLUG"
fi

say "${STEPS[5]}"
go_on "run clusia tests $PR --run-scenarios --scenario $SLUG (runs the PR's code on this Mac)"
clusia tests "$PR" --run-scenarios --scenario "$SLUG"
code=$?
echo "  clusia exited $code (0 passed, 1 failed or erred, 124 timed out, 130 stopped)."
ask "The exit code matches the stream above"

say "${STEPS[6]}"
go_on "prepare main: its worktree and caches take disk of their own"
clusia tests "$PR" --run-scenarios --scenario "$SLUG" --on-main
check "main's worktree exists" test -d "$DATA/worktrees/.main/$KEY"

say "${STEPS[7]}"
clusia tests "$PR" --scenarios
ask "The table's Δ labels follow the rule (a gap inside the spread or under 5% is ≈) and a verdict line sums it up"

say "${STEPS[8]}"
go_on "start a comparison and press Ctrl-C while a round runs"
clusia tests "$PR" --run-scenarios --scenario "$SLUG" --on-main
code=$?
echo "  clusia exited $code."
check "the scenario says the verdict is 'stopped: no verdict'" \
  bash -c "clusia tests '$PR' --scenarios | grep -q 'stopped: no verdict'"
ask "The exit code was 130 and the finished rounds were kept"

say "${STEPS[9]}"
echo "  Name a changed function of the pull request (as clusia tests $PR --functions lists it)."
read -r -p "  Function: " NAME || NAME=""
if [ -n "$NAME" ]; then
  go_on "run clusia tests $PR --bench '$NAME' --write (one agent turn on your account)"
  clusia tests "$PR" --bench "$NAME" --write
  go_on "run clusia tests $PR --bench '$NAME' --on-main"
  clusia tests "$PR" --bench "$NAME" --on-main
  ask "p50, p95 and calls/s appear for both sides"
else
  echo "  Skipped: no function named."
fi

say "${STEPS[10]}"
go_on "run clusia tests $PR --copilot explain --scenario $SLUG (one agent turn on your account)"
clusia tests "$PR" --copilot explain --scenario "$SLUG"
go_on "run clusia tests $PR --copilot 'what next?' (one agent turn on your account)"
clusia tests "$PR" --copilot "what next?"
ask "Both answers are plain text, and any suggestion is a line of text"

say "${STEPS[11]}"
clusia tests "$PR" --as-comment "$SLUG"
ask "A suggested comment was printed, and nothing was posted"

say "${STEPS[12]}"
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
if [ "$(clusia config get tests.runner 2>/dev/null)" = "$OLD_RUNNER" ]; then
  pass "tests.runner is as it was"
else
  fail "tests.runner is as it was"
fi

printf '\n%d passed, %d failed\n' "$passed" "$failed"
[ "$failed" = 0 ]
