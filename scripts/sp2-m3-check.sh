#!/usr/bin/env bash
# Owner check for the security and audit checks: your own Claude Code, on a real pull request of
# yours, with the window in front of you. It spends some of your Claude Code usage (each check
# is one turn on your account), opens a real review of the pull request, and discards that
# review twice: in step 6, to see the tabs of a review that has no results, and at the end.
# Nothing is published to GitHub. Clusia asks you before the agent runs a command, as always,
# and the script never answers for you.
#
#   scripts/sp2-m3-check.sh --pr rzorzal/clusia#123     run the check (asks first)
#   scripts/sp2-m3-check.sh --list                      print the steps and exit
#   add --yes to skip the question below; every step still asks you on the terminal, and
#   without one each of those questions is answered no
#
# Needs Clusia installed (clusia on the PATH, signed in to GitHub) and Claude Code installed and
# signed in. The window is started for you; look at it when a step says so. If the pull request
# already has a review of yours on this Mac, its draft is lost in step 6: use a pull request
# you do not mind opening fresh.
set -u

PR=""
LIST=0
ASSUME_YES=0
passed=0
failed=0
ERR="$(mktemp -t sp2-m3-check.XXXXXX)"
RUN="$(date +%s)"
OLD_ON_OPEN=""
OLD_SECURITY=""
OLD_AUDIT=""
OLD_AREAS=""
CHANGED=0
APP_PID=""
ASK_PID=""
# Set to wait at most this many seconds at every step, instead of each step's own time.
WAIT_CAP="${SP2_CHECK_WAIT:-}"

restore_settings() {
  # The check changes four settings; the owner's own values come back when it ends.
  [ "$CHANGED" = 1 ] || return 0
  [ -n "$OLD_ON_OPEN" ] && clusia config set harness.on_open "$OLD_ON_OPEN" >/dev/null 2>&1
  [ -n "$OLD_SECURITY" ] && clusia config set harness.check_security "$OLD_SECURITY" >/dev/null 2>&1
  [ -n "$OLD_AUDIT" ] && clusia config set harness.audit "$OLD_AUDIT" >/dev/null 2>&1
  [ -n "$OLD_AREAS" ] && clusia config set harness.audit_areas "$OLD_AREAS" >/dev/null 2>&1
  if [ -z "$OLD_ON_OPEN" ] || [ -z "$OLD_SECURITY" ] || [ -z "$OLD_AUDIT" ] || [ -z "$OLD_AREAS" ]; then
    echo "Some old values could not be read. Check Config › Harness: When I open a review (three boxes) and the audit areas (remove clusia-check-$RUN if it is still there)." >&2
  fi
}
close_window() {
  if [ -n "$APP_PID" ]; then kill "$APP_PID" 2>/dev/null; fi
}
stop_ask() {
  if [ -n "$ASK_PID" ]; then kill "$ASK_PID" 2>/dev/null; fi
}
trap 'stop_ask; close_window; restore_settings; rm -f "$ERR" "$ERR.out"' EXIT

STEPS=(
  "preflight: claude and clusia are installed, the daemon answers and the three review checks are on"
  "open: both checks run when the review opens, and the chat answers while they run"
  "accept: Add to draft puts a finding in the draft, marked from Security"
  "dismiss: a dismissed finding does not come back on Check again"
  "custom: a custom audit area appears in the Audits tab and is checked"
  "off: with the boxes off, the tabs offer Run instead of results"
  "cli: clusia check runs a check and prints its findings"
  "end: discarding the review stops what runs and deletes the results"
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
  echo "This opens a real review of $PR and asks YOUR Claude Code (it uses your plan) to check"
  echo "the change for security problems and to audit it. You must answer every request for a"
  echo "command yourself: the script never answers for you. It changes four Config › Harness"
  echo "settings for the run and puts them back, and it discards the review of $PR on this Mac"
  echo "twice (step 6 and the end), so a draft you have there is lost. It publishes nothing to GitHub."
  printf 'Continue? [y/N] '
  read -r answer || answer=n
  case "$answer" in y | Y | yes) return 0 ;; *) return 1 ;; esac
}

# Draft items that came from a check, as the terminal prints them in JSON.
check_items() { clusia --json review status "$PR" 2>/dev/null | grep -Eio '"origin": ?"(security|audit)"' | wc -l | tr -d ' '; }

# Asks the agent without a keyboard, so a request for a command waits for the window and the
# script can never answer it. `wait_ask` ends it.
ask_agent_detached() {
  clusia ask "$PR" "$1" </dev/null >"$ERR.out" 2>"$ERR" &
  ASK_PID=$!
}
wait_ask() {
  [ -n "$ASK_PID" ] && wait "$ASK_PID" 2>/dev/null
  ASK_PID=""
}

app_binary() {
  local dir
  # Set to run another window program instead of the installed one.
  if [ -n "${SP2_CHECK_APP:-}" ]; then
    [ -x "$SP2_CHECK_APP" ] && echo "$SP2_CHECK_APP"
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
  check "Claude Code is installed" command -v claude
  check "Claude Code answers" claude --version
  check "clusia is installed" command -v clusia
  check "the daemon answers (it is started if needed)" clusia daemon start
  OLD_ON_OPEN="$(clusia config get harness.on_open 2>/dev/null)"
  OLD_SECURITY="$(clusia config get harness.check_security 2>/dev/null)"
  OLD_AUDIT="$(clusia config get harness.audit 2>/dev/null)"
  OLD_AREAS="$(clusia config get harness.audit_areas 2>/dev/null)"
  echo "  The check turns all three boxes of 'When I open a review' on. Your values"
  echo "  (summarize '$OLD_ON_OPEN', security '$OLD_SECURITY', audit '$OLD_AUDIT', and your audit areas)"
  echo "  are put back when it ends."
  CHANGED=1
  clusia config set harness.on_open summarize >/dev/null 2>&1
  clusia config set harness.check_security true >/dev/null 2>&1
  clusia config set harness.audit true >/dev/null 2>&1
  check "the security check is on" test "$(clusia config get harness.check_security 2>/dev/null)" = "true"
  check "the audit is on" test "$(clusia config get harness.audit 2>/dev/null)" = "true"
  check "$PR is a pull request clusia can open" clusia open "$PR"
  start_window
}

step_open() {
  say "2. Both checks run when the review opens"
  echo "  Look at the window: after the summary, the Security and Audits tabs should show a"
  echo "  running mark. While they run, the script asks the chat a question."
  ask_agent_detached "What is 6 times 7? Answer with only the number."
  wait_for "the chat answered while the checks run (it said 42)" 600 grep -q "42" "$ERR.out"
  wait_ask
  ask "Did the Security and Audits tabs show a running mark while the chat answered?"
  echo "  Wait until both tabs stop running (a count, a check mark or a result)."
  ask "Did the Security tab end with a number of findings or a clean result worded 'found no security issues'?"
  ask "Did the Audits tab list your audit areas, each with a count, 'ok' or 'Not checked'?"
}

step_accept() {
  local before after
  say "3. Add to draft"
  before="$(check_items)"
  echo "  In the Security tab, click Add to draft on one finding (if there is none, use Audits >"
  echo "  Accept into draft). The card should read 'In your draft'."
  wait_for "a draft item from a check appeared" 120 test "$(check_items)" -gt "$before"
  after="$(check_items)"
  ask "Does the draft card say 'from Security' (or 'from Audit') and does the finding's card say 'In your draft'?"
  echo "  Items from checks before: $before, now: $after."
}

step_dismiss() {
  say "4. Dismiss and Check again"
  echo "  Dismiss one finding in the Security tab and remember its title. Then click Check again"
  echo "  and wait for it to finish."
  ask "Did the dismissed finding stay away after the new check (the agent may word a different one)?"
}

step_custom() {
  say "5. A custom audit area"
  echo "  In Config › Harness › Audit areas click + Add area. Name it clusia-check-$RUN and write the"
  echo "  instruction: Look for a function whose name starts with refresh and name its file."
  wait_for "the area is in your settings" 180 sh -c "clusia config get harness.audit_areas | grep -q 'clusia-check-$RUN'"
  echo "  Back in the review, open Audits and click Check again."
  ask "Did the Audits list show clusia-check-$RUN, and did it end with findings, passes or 'ok' (not stuck on Not checked after the run)?"
}

step_off() {
  say "6. With the boxes off the tabs offer Run"
  echo "  This discards the review of $PR on this Mac (its draft items are lost) and turns the"
  echo "  summary, security and audit boxes off, so nothing runs when it opens again."
  if ! yes_or_no "Discard it now?"; then
    echo "  Skipped."
    return
  fi
  clusia review discard "$PR" >/dev/null 2>"$ERR" || fail "the review was not discarded: $(cat "$ERR")"
  clusia config set harness.on_open wait >/dev/null 2>&1
  clusia config set harness.check_security false >/dev/null 2>&1
  clusia config set harness.audit false >/dev/null 2>&1
  check "the security box is off" test "$(clusia config get harness.check_security 2>/dev/null)" = "false"
  check "$PR opens again" clusia open "$PR"
  ask "Do the Security and Audits tabs offer 'Run security check' and 'Run audit', with no results and nothing running?"
}

step_cli() {
  local status
  say "7. clusia check"
  echo "  This runs the security check at the terminal and prints what it found."
  clusia check "$PR" --security >"$ERR.out" 2>"$ERR"
  status=$?
  cat "$ERR.out"
  if [ "$status" -eq 0 ]; then pass "clusia check finished (exit 0, even with findings)"; else fail "clusia check failed with exit $status: $(cat "$ERR")"; fi
  if grep -Eq '^(HIGH|MEDIUM|LOW)  [^ ]+  ' "$ERR.out" || grep -q 'found no security issues' "$ERR.out"; then
    pass "it printed findings as SEVERITY file:line  title, or said it found no security issues"
  else
    fail "its output has no finding line and no 'found no security issues'"
  fi
  check "a bad option is a usage error (exit 2)" sh -c 'clusia check "$0" --nope >/dev/null 2>&1; test $? -eq 2' "$PR"
}

step_end() {
  say "8. The end"
  echo "  This discards the review of $PR on this Mac (its draft items are lost) and deletes the"
  echo "  results of the checks. This check does not publish."
  if ! yes_or_no "Discard it now?"; then
    echo "  Skipped: the review was kept, with its draft. Discard it later with: clusia review discard $PR"
    return
  fi
  if clusia review discard "$PR" >/dev/null 2>"$ERR"; then
    pass "the review was discarded"
  else
    fail "the review was not discarded: $(cat "$ERR")"
  fi
  echo "  Opening it again starts clean: the tabs offer Run (the boxes are put back after this)."
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
step_open
step_accept
step_dismiss
step_custom
step_off
step_cli
step_end
summary
