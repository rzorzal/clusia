#!/usr/bin/env bash
# Owner check for Custom command as the third harness. It runs the example program
# examples/harness/echo.py, which calls no model and no network, so it spends nothing. Use a demo
# pull request on rzorzal/* (a repository you do not mind opening a review of), with the window in
# front of you. The script opens a real review, and asks you to do the human parts yourself:
# answer permission requests, push a commit and discard the review. It pushes and publishes
# nothing, and it never answers a permission request.
#
#   scripts/sp2-m6-check.sh --pr rzorzal/clusia#123     run the check (asks first)
#   scripts/sp2-m6-check.sh --list                      print the steps and exit
#   add --yes to skip the first question below; the script still asks on the terminal before it
#   changes your settings, and every step's own question is still yours to answer (a question
#   without an answer counts as no)
#
# Needs Clusia installed (clusia on the PATH, signed in to GitHub) and python3. The program the
# review uses is a tiny wrapper the script writes in a temporary folder: it records the
# environment it was started with, then runs echo.py. If the pull request already has a review of
# yours on this Mac, its draft is part of the check: use a pull request you do not mind opening
# fresh.
set -u

PR=""
LIST=0
ASSUME_YES=0
passed=0
failed=0
ERR="$(mktemp -t sp2-m6-check.XXXXXX)"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ECHO="$(cd "$SCRIPT_DIR/.." && pwd)/examples/harness/echo.py"
WORK=""
WRAPPER=""
ENVLOG=""
OLD_KIND=""
OLD_PROGRAM=""
PROGRAM_READ=0
OLD_NAME=""
NAME_READ=0
OLD_REVALIDATE=""
OLD_QUIET=""
OLD_ON_OPEN=""
OLD_SECURITY=""
OLD_AUDIT=""
CHANGED=0
APP_PID=""
ASK_PID=""
BASELINE_PROCS=0
# Set to wait at most this many seconds at every step, instead of each step's own time.
WAIT_CAP="${SP2_CHECK_WAIT:-}"

restore_settings() {
  # The check changes eight settings; the owner's own values come back when it ends. The kind
  # goes back first: a change of kind clears the program and the name, so they are written after.
  [ "$CHANGED" = 1 ] || return 0
  [ -n "$OLD_KIND" ] && clusia config set harness.kind "$OLD_KIND" >/dev/null 2>&1
  [ "$PROGRAM_READ" = 1 ] && clusia config set harness.program "$OLD_PROGRAM" >/dev/null 2>&1
  [ "$NAME_READ" = 1 ] && clusia config set harness.custom_name "$OLD_NAME" >/dev/null 2>&1
  [ -n "$OLD_REVALIDATE" ] && clusia config set harness.revalidate "$OLD_REVALIDATE" >/dev/null 2>&1
  [ -n "$OLD_QUIET" ] && clusia config set harness.revalidate_quiet_secs "$OLD_QUIET" >/dev/null 2>&1
  [ -n "$OLD_ON_OPEN" ] && clusia config set harness.on_open "$OLD_ON_OPEN" >/dev/null 2>&1
  [ -n "$OLD_SECURITY" ] && clusia config set harness.check_security "$OLD_SECURITY" >/dev/null 2>&1
  [ -n "$OLD_AUDIT" ] && clusia config set harness.audit "$OLD_AUDIT" >/dev/null 2>&1
  [ -n "$OLD_KIND" ] || echo "Could not read the old harness.kind; check Config › Harness: which agent is selected." >&2
  [ "$PROGRAM_READ" = 1 ] || echo "Could not read the old harness.program; check Config › Harness: Program." >&2
  [ "$NAME_READ" = 1 ] || echo "Could not read the old harness.custom_name; check Config › Harness: Name." >&2
  [ -n "$OLD_REVALIDATE" ] || echo "Could not read the old harness.revalidate; check Config › Harness: Revalidate my draft." >&2
  [ -n "$OLD_QUIET" ] || echo "Could not read the old harness.revalidate_quiet_secs; check Config › Harness: Wait for pushes to settle." >&2
  [ -n "$OLD_ON_OPEN" ] || echo "Could not read the old harness.on_open; check Config › Harness: When I open a review." >&2
  [ -n "$OLD_SECURITY" ] || echo "Could not read the old harness.check_security; check Config › Harness: Check security." >&2
  [ -n "$OLD_AUDIT" ] || echo "Could not read the old harness.audit; check Config › Harness: Audit." >&2
  return 0
}
# Stops a `clusia ask` still waiting for the owner, so the next one starts alone.
end_ask() {
  if [ -n "$ASK_PID" ]; then
    kill "$ASK_PID" 2>/dev/null
    wait "$ASK_PID" 2>/dev/null
    ASK_PID=""
  fi
}
close_window() {
  if [ -n "$APP_PID" ]; then kill "$APP_PID" 2>/dev/null; fi
}
drop_work() {
  if [ -n "$WORK" ]; then find "$WORK" -delete 2>/dev/null; fi
}
trap 'end_ask; close_window; restore_settings; drop_work; rm -f "$ERR" "$ERR".*' EXIT
# clusia exits 130 on a Ctrl-C by status, not by the signal, so bash would go on to the next
# step; the script ends instead (the EXIT trap still cleans up).
trap 'exit 130' INT TERM HUP

STEPS=(
  "preflight: python3 and clusia are installed, clusia harness test passes on examples/harness/echo.py, and Config › Harness is switched to a custom command named Echo before the review opens"
  "switch: Config › Harness shows Custom command selected with the amber notice, no sandbox rows, and Test answers Echo with its version and protocol 1"
  "open: the review opens and the status pill and the Agent panel say Echo"
  "chat: a question is answered by Echo, a second message works in the same session, and the chat goes on after you close and reopen the review"
  "isolation: no GitHub token and no CLUSIA_ variable reached the program"
  "approve: a command Echo asks to run asks you, and Deny, Allow once and the timeout each behave"
  "checks: Security and Audit run on Echo and finish with no findings"
  "revalidate: you save the review, push a commit, and Echo revalidates the draft"
  "end: you discard the review and no echo.py process is left running"
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
  echo "This opens a real review of $PR and runs examples/harness/echo.py as a custom command:"
  echo "a program that calls no model and no network, so nothing is spent."
  echo "You must answer every request for a command yourself: the script never answers for you."
  echo "It changes eight Config › Harness settings for the run and puts them back."
  echo "It pushes nothing: you push the test commit to the pull request's branch, and you discard"
  echo "the review at the end (nothing is published)."
  printf 'Continue? [y/N] '
  read -r answer || answer=n
  case "$answer" in y | Y | yes) return 0 ;; *) return 1 ;; esac
}

echo_procs() { pgrep -f 'harness/echo.py' 2>/dev/null | wc -l | tr -d ' '; }
# A function, so each poll of wait_for counts again.
no_new_procs() { [ "$(echo_procs)" -le "$BASELINE_PROCS" ]; }
said_in() { grep -qF -- "$1" "$2"; }
log_has() { clusia agent log "$PR" 2>/dev/null | grep -qF "$1"; }

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

# The wrapper records the environment each start of the program sees, then becomes echo.py.
write_wrapper() {
  WORK="$(mktemp -d -t sp2-m6-work.XXXXXX)"
  WRAPPER="$WORK/echo-wrapper.sh"
  ENVLOG="$WORK/env.log"
  {
    echo '#!/bin/sh'
    echo "env >> \"$ENVLOG\""
    echo "exec python3 \"$ECHO\" \"\$@\""
  } >"$WRAPPER"
  chmod 755 "$WRAPPER"
}

step_preflight() {
  local version
  say "1. Preflight"
  if ! version="$(python3 --version 2>&1)"; then
    echo "  python3 --version failed, so the example program cannot run on this machine: $version"
    echo "  Run this check where python3 works. Nothing was changed."
    exit 1
  fi
  pass "python3 --version answers: $version"
  check "examples/harness/echo.py is there and executable" test -x "$ECHO"
  check "clusia is installed" command -v clusia
  check "the daemon answers (it is started if needed)" clusia daemon start
  check "clusia harness test passes on echo.py, with the permission round trip" clusia harness test "$ECHO" --permission
  BASELINE_PROCS="$(echo_procs)"
  OLD_KIND="$(clusia config get harness.kind 2>/dev/null)"
  if OLD_PROGRAM="$(clusia config get harness.program 2>/dev/null)"; then PROGRAM_READ=1; fi
  if OLD_NAME="$(clusia config get harness.custom_name 2>/dev/null)"; then NAME_READ=1; fi
  OLD_REVALIDATE="$(clusia config get harness.revalidate 2>/dev/null)"
  OLD_QUIET="$(clusia config get harness.revalidate_quiet_secs 2>/dev/null)"
  OLD_ON_OPEN="$(clusia config get harness.on_open 2>/dev/null)"
  OLD_SECURITY="$(clusia config get harness.check_security 2>/dev/null)"
  OLD_AUDIT="$(clusia config get harness.audit 2>/dev/null)"
  echo "  The check selects Custom command (which clears the program path and the name, as any"
  echo "  change of agent does), points it at a wrapper around echo.py, names it Echo before the"
  echo "  review opens (so the review is pinned to that name), turns revalidation on with a"
  echo "  60 second wait and turns the three 'When I open a review' boxes off, so the only turns it"
  echo "  runs are the ones you see here. Your values (agent '$OLD_KIND', program '$OLD_PROGRAM',"
  echo "  name '$OLD_NAME', revalidate '$OLD_REVALIDATE', wait '$OLD_QUIET' s, on open '$OLD_ON_OPEN',"
  echo "  security '$OLD_SECURITY', audit '$OLD_AUDIT') are put back when it ends."
  yes_or_no "Change those eight settings now (they are put back when the check ends)?" || {
    echo "Nothing was changed."
    exit 1
  }
  write_wrapper
  CHANGED=1
  clusia config set harness.kind custom >/dev/null 2>&1
  clusia config set harness.program "$WRAPPER" >/dev/null 2>&1
  clusia config set harness.custom_name Echo >/dev/null 2>&1
  clusia config set harness.revalidate true >/dev/null 2>&1
  clusia config set harness.revalidate_quiet_secs 60 >/dev/null 2>&1
  clusia config set harness.on_open wait >/dev/null 2>&1
  clusia config set harness.check_security false >/dev/null 2>&1
  clusia config set harness.audit false >/dev/null 2>&1
  check "Custom command is the selected agent" test "$(clusia config get harness.kind 2>/dev/null)" = "custom"
  check "the name is Echo" test "$(clusia config get harness.custom_name 2>/dev/null)" = "Echo"
  check "the program is the wrapper around echo.py" test "$(clusia config get harness.program 2>/dev/null)" = "$WRAPPER"
  check "revalidation is on" test "$(clusia config get harness.revalidate 2>/dev/null)" = "true"
  check "$PR is a pull request clusia can open" clusia open "$PR"
  start_window
}

step_switch() {
  say "2. Config › Harness"
  echo "  In the window open Config › Harness."
  ask "Is the Custom command card selected, with an amber notice that Clúsia cannot sandbox it, a How to write one link, and no Sandbox or Claude settings rows?"
  echo "  Press Test."
  ask "Does it read 'Echo 1.0 answered in … s · protocol 1', with the Name field showing Echo?"
}

step_open() {
  say "3. Open the review"
  echo "  In the window open the review of $PR (it is already open if the window started on it)."
  ask "Does the status bar say 'Echo · …' and the Agent panel header say Echo, never Claude Code?"
}

step_chat() {
  say "4. Chat"
  clusia ask "$PR" "Say hello to the reviewer." </dev/null >"$ERR.out" 2>"$ERR"
  if said_in "Echo: Say hello to the reviewer." "$ERR.out"; then pass "the chat was answered by Echo"; else fail "the chat was not answered by Echo: $(cat "$ERR.out" "$ERR")"; fi
  clusia ask "$PR" "And a second message." </dev/null >"$ERR.out.more" 2>"$ERR"
  if said_in "Echo: And a second message." "$ERR.out.more"; then pass "a second message in the same session was answered"; else fail "the second message was not answered: $(cat "$ERR.out.more" "$ERR")"; fi
  ask "Did each answer appear once in the chat, with no repeated or doubled text?"
  echo "  Now close the review's tab (Keep for later) and open it again from Home."
  ask "Does the chat still show both questions and their answers, and does the panel say the session resumed?"
  check "agent log prints the chat" log_has "Say hello to the reviewer."
}

step_isolation() {
  say "5. No token reaches the program"
  if [ -s "$ENVLOG" ]; then pass "the program ran and recorded its environment"; else fail "the program recorded no environment"; fi
  if grep -Eq '^(GH_TOKEN|GITHUB_TOKEN|GH_ENTERPRISE_TOKEN|CLUSIA_)' "$ENVLOG" 2>/dev/null; then
    fail "a GitHub token or a CLUSIA_ variable reached the program"
  else
    pass "no GH_TOKEN, GITHUB_TOKEN, GH_ENTERPRISE_TOKEN or CLUSIA_ variable reached the program"
  fi
}

step_approve() {
  say "6. A command asks you"
  clusia ask "$PR" "Ask permission to run the command true, then say what happened." </dev/null >"$ERR.deny.out" 2>"$ERR.deny" &
  ASK_PID=$!
  echo "  The window shows 'Echo wants to run a command'. Choose Deny, and watch the chat go on."
  wait_for "Echo read the answer: Permission denied" 120 said_in "Permission denied" "$ERR.deny.out"
  end_ask
  clusia ask "$PR" "Ask permission to run the command true, once more." </dev/null >"$ERR.allow.out" 2>"$ERR.allow" &
  ASK_PID=$!
  echo "  Choose Allow once this time."
  wait_for "Echo read the answer: Permission allowed" 120 said_in "Permission allowed" "$ERR.allow.out"
  end_ask
  clusia ask "$PR" "Ask permission to run the command true, and wait." </dev/null >"$ERR.late.out" 2>"$ERR.late" &
  ASK_PID=$!
  echo "  This time do not answer. The request is denied when the time in Config › Harness passes."
  wait_for "the unanswered request was denied by itself" 300 said_in "Permission denied" "$ERR.late.out"
  end_ask
  ask "Was there a notification for the unanswered request and a line in the chat saying nobody answered in time?"
}

step_checks() {
  local status
  say "7. Security and Audit"
  clusia check "$PR" --security >"$ERR.out.sec" 2>"$ERR"
  status=$?
  cat "$ERR.out.sec"
  if [ "$status" -eq 0 ]; then pass "clusia check --security finished (exit 0)"; else fail "clusia check --security failed with exit $status: $(cat "$ERR")"; fi
  clusia check "$PR" --audit >"$ERR.out.audit" 2>"$ERR"
  status=$?
  cat "$ERR.out.audit"
  if [ "$status" -eq 0 ]; then pass "clusia check --audit finished (exit 0)"; else fail "clusia check --audit failed with exit $status: $(cat "$ERR")"; fi
  ask "Did the Security and Audits tabs show results that say Echo, with no findings?"
}

step_revalidate() {
  local status
  say "8. Revalidation"
  echo "  In the window add one comment on a line of the Diff, close the review's tab and choose"
  echo "  Keep for later. Then, in a clone of the pull request's branch, push one commit that changes"
  echo "  the commented code (you push, not this script). Wait about two minutes."
  ask "Did exactly one notification arrive, saying the review was revalidated?"
  ask "Does the revalidation screen say Echo checked your comment against the new commit, with a verdict row for it?"
  clusia revalidate "$PR" --again </dev/null >"$ERR.out" 2>"$ERR"
  status=$?
  cat "$ERR.out"
  if [ "$status" -eq 0 ]; then pass "clusia revalidate --again finished (exit 0)"; else fail "clusia revalidate failed with exit $status: $(cat "$ERR")"; fi
}

step_end() {
  say "9. The end"
  if clusia review discard "$PR" >/dev/null 2>"$ERR"; then
    pass "the review was discarded"
  else
    fail "the review was not discarded: $(cat "$ERR")"
  fi
  wait_for "no echo.py process is left running" 15 no_new_procs
  ask "Did nothing reach GitHub from this check (no comment, no review)?"
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
case "$PR" in
  rzorzal/*) ;;
  *) echo "Warning: $PR is not on rzorzal/*" >&2 ;;
esac
confirm || {
  echo "Nothing was run."
  exit 1
}
step_preflight
step_switch
step_open
step_chat
step_isolation
step_approve
step_checks
step_revalidate
step_end
summary
