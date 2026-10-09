#!/usr/bin/env bash
# Owner check for agent permissions: your own Claude Code, on a real pull request of yours, with
# the window in front of you. The agent is asked to run small, harmless commands (touch, mkdir,
# ln, chmod and one network try) inside the review's worktree; YOU answer every request: in the
# window, from the notification, or at the terminal. It spends a little of your Claude Code
# usage, opens a real review of the pull request, and discards that review at the end. Nothing
# is published to GitHub. The files and folders it asks for are named clusia-check-<run>-*
# and stay untracked in the review's worktree; step 3 also creates three local branches
# named clusia-check-<run>-a, -b and -c, which you can delete with git branch -D.
#
#   scripts/sp2-m2-check.sh --pr rzorzal/clusia#123     run the check (asks first)
#   scripts/sp2-m2-check.sh --list                      print the steps and exit
#   add --yes to skip the question below; every step still asks you on the terminal, and
#   without one each of those questions is answered no
#
# Needs Clusia installed (clusia on the PATH, signed in to GitHub) and Claude Code installed and
# signed in. The window is started for you; look at it when a step says so.
set -u

PR=""
LIST=0
ASSUME_YES=0
passed=0
failed=0
ERR="$(mktemp -t sp2-m2-check.XXXXXX)"
RUN="$(date +%s)"
OLD_ON_OPEN=""
OLD_TIMEOUT=""
OLD_SANDBOX=""
CHANGED=0
APP_PID=""
ASK_PID=""
# Set to wait at most this many seconds at every step, instead of each step's own time.
WAIT_CAP="${SP2_CHECK_WAIT:-}"

restore_settings() {
  # The check changes three settings; the owner's own values come back when it ends.
  [ "$CHANGED" = 1 ] || return 0
  [ -n "$OLD_ON_OPEN" ] && clusia config set harness.on_open "$OLD_ON_OPEN" >/dev/null 2>&1
  [ -n "$OLD_TIMEOUT" ] && clusia config set harness.permission_timeout_secs "$OLD_TIMEOUT" >/dev/null 2>&1
  [ -n "$OLD_SANDBOX" ] && clusia config set harness.sandbox "$OLD_SANDBOX" >/dev/null 2>&1
  if [ -z "$OLD_ON_OPEN" ] || [ -z "$OLD_TIMEOUT" ] || [ -z "$OLD_SANDBOX" ]; then
    echo "Some old values could not be read. Check Config › Harness: When I open a review, Deny unanswered requests after, and the sandbox switch." >&2
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
  "preflight: claude and clusia are installed, the daemon answers and the sandbox is on"
  "once: a command the agent may not run asks, and Allow once runs it"
  "review: Allow for this review runs the next matching command without asking, and the x on its chip takes it back"
  "deny: Deny shows in the chat and the agent goes on without the command"
  "expire: an unanswered request is denied when its time runs out"
  "slow: an answer after about 100 seconds still works, at the default 120 second wait"
  "notification: with the window closed, the notification opens the window on the question"
  "sandbox: with the sandbox on, a command that needs the network is blocked"
  "terminal: clusia ask asks at the terminal and the answer there works"
  "end: discarding the review ends the session and keeps the log"
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
  echo "This opens a real review of $PR and asks YOUR Claude Code (it uses your plan) to run"
  echo "a few small commands in that review's worktree. You must answer every request yourself:"
  echo "the script never answers for you. It changes three Config › Harness settings for the run"
  echo "and puts them back, and it discards the review at the end. It publishes nothing to GitHub."
  printf 'Continue? [y/N] '
  read -r answer || answer=n
  case "$answer" in y | Y | yes) return 0 ;; *) return 1 ;; esac
}

# The chat of the review, as the terminal prints it (permission lines start with ✓ or ⊘).
log_has() { clusia agent log "$PR" 2>/dev/null | grep -qF "$1"; }

# Asks the agent without a keyboard, so a request waits for the window or the notification and
# the script can never answer it. `wait_ask` ends it.
ask_agent_detached() {
  clusia ask "$PR" "$1" </dev/null >"$ERR.out" 2>"$ERR" &
  ASK_PID=$!
}
wait_ask() {
  [ -n "$ASK_PID" ] && wait "$ASK_PID" 2>/dev/null
  ASK_PID=""
}
command_prompt() {
  echo "Run exactly this shell command in the worktree and nothing else: $1. Then say done in one word."
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
  OLD_TIMEOUT="$(clusia config get harness.permission_timeout_secs 2>/dev/null)"
  OLD_SANDBOX="$(clusia config get harness.sandbox 2>/dev/null)"
  echo "  The check waits for your first question instead of summarizing, and sets the sandbox on."
  echo "  Your values (on_open '$OLD_ON_OPEN', timeout '$OLD_TIMEOUT', sandbox '$OLD_SANDBOX') are put back when it ends."
  CHANGED=1
  clusia config set harness.on_open wait >/dev/null 2>&1
  clusia config set harness.sandbox true >/dev/null 2>&1
  check "the sandbox is on" test "$(clusia config get harness.sandbox 2>/dev/null)" = "true"
  check "$PR is a pull request clusia can open" clusia open "$PR"
  start_window
}

step_once() {
  say "2. Allow once"
  ask_agent_detached "$(command_prompt "touch clusia-check-$RUN-once.txt")"
  echo "  The window shows 'Claude Code wants to run a command' with touch clusia-check-$RUN-once.txt."
  ask "Did the modal appear, with the exact command, a countdown, and Deny and Allow once (a 'for this review' button may also be there: do not click it)?"
  echo "  Click Allow once."
  wait_for "the agent ran it (the log says you allowed it)" 90 log_has "✓ ran touch clusia-check-$RUN-once.txt (you allowed it)"
  wait_ask
  ask "Did the chat show '✓ ran … (you allowed it)' and did the modal close?"
}

step_review() {
  say "3. Allow for this review"
  echo "  These steps create local branches named clusia-check-$RUN-* in the review's repository;"
  echo "  delete them afterwards with: git branch -D <name>"
  ask_agent_detached "$(command_prompt "git branch clusia-check-$RUN-a")"
  echo "  The modal offers 'Allow git branch … for this review'. Click it."
  wait_for "the agent ran it (the log says allowed for this review)" 90 log_has "✓ ran git branch clusia-check-$RUN-a (allowed for this review)"
  wait_ask
  ask "Does the chat footer list 'git branch' under 'Allowed for this review'?"
  ask_agent_detached "$(command_prompt "git branch clusia-check-$RUN-b")"
  echo "  This one matches the rule: no modal should appear."
  wait_for "the second command ran without asking" 90 log_has "✓ ran git branch clusia-check-$RUN-b (allowed for this review)"
  wait_ask
  ask "Did the second command run WITHOUT a modal?"
  echo "  Now click the x on the 'git branch' chip."
  ask "Did the chip go away?"
  ask_agent_detached "$(command_prompt "git branch clusia-check-$RUN-c")"
  echo "  With the rule gone the modal comes back. Click Deny."
  wait_for "the request came back and was denied" 90 log_has "⊘ you denied git branch clusia-check-$RUN-c"
  wait_ask
}

step_deny() {
  say "4. Deny"
  ask_agent_detached "$(command_prompt "mkdir clusia-check-$RUN-dir.d")"
  echo "  Click Deny in the modal."
  wait_for "the log says you denied it" 90 log_has "⊘ you denied mkdir clusia-check-$RUN-dir.d"
  wait_ask
  ask "Did the chat show '⊘ you denied …' and did the agent answer without the command?"
}

step_expire() {
  say "5. An unanswered request"
  echo "  The time is set to 30 seconds for this step. Do NOT answer the request."
  clusia config set harness.permission_timeout_secs 30 >/dev/null 2>&1
  ask_agent_detached "$(command_prompt "ln -s /dev/null clusia-check-$RUN-link")"
  wait_for "the request expired (the log says no answer in time)" 90 log_has "⊘ denied ln -s /dev/null clusia-check-$RUN-link: no answer in time"
  wait_ask
  clusia config set harness.permission_timeout_secs 120 >/dev/null 2>&1
  ask "Did the modal show a countdown and close by itself, with '⊘ denied …: no answer in time' in the chat?"
}

step_slow() {
  say "6. A slow answer"
  echo "  The time is 120 seconds (the default) for this step. Watch the countdown and WAIT about"
  echo "  100 seconds before you click Allow once: Claude Code must still be waiting for the answer."
  ask_agent_detached "$(command_prompt "mkdir clusia-check-$RUN-slow.d")"
  wait_for "the late answer worked (the log says you allowed it)" 150 log_has "✓ ran mkdir clusia-check-$RUN-slow.d (you allowed it)"
  wait_ask
  ask "Did the agent finish normally after the late answer (no timeout error in the chat)?"
}

step_notification() {
  say "7. The notification"
  close_window
  APP_PID=""
  echo "  The window was closed. Keep Clúsia's notifications on (Config › Notifications)."
  ask_agent_detached "$(command_prompt "chmod 644 clusia-check-$RUN-once.txt")"
  echo "  A notification 'Agent needs your permission' should appear. Click it."
  ask "Did the notification appear, and did clicking it open the window on the question (the modal is there)?"
  echo "  Click Deny in the modal."
  wait_for "the request was denied" 120 log_has "⊘ you denied chmod 644 clusia-check-$RUN-once.txt"
  wait_ask
  echo "  Close the window yourself when you are done looking at it."
}

step_sandbox() {
  say "8. The sandbox"
  ask_agent_detached "Try to fetch https://example.com with the curl program, using --max-time 5, and tell me whether it worked."
  echo "  The modal says 'no network'. Click Allow once."
  wait_for "the command was allowed once" 90 log_has "✓ ran curl"
  wait_ask
  ask "Did the agent say the request failed (no network), and did the modal name the sandbox?"
}

step_terminal() {
  say "9. The terminal"
  echo "  clusia ask now asks here. Type o and press Enter when it does."
  clusia ask "$PR" "$(command_prompt "mkdir clusia-check-$RUN-tty.d")"
  local status=$?
  if [ "$status" -eq 130 ]; then
    echo "Interrupted: the check stops here."
    exit 130
  fi
  ask "Did it print 'Claude Code wants to run: mkdir …  [o]nce / [r]eview (mkdir) / [d]eny? ' and run the command after your o?"
  if log_has "✓ ran mkdir clusia-check-$RUN-tty.d (you allowed it)"; then pass "the log says you allowed it"; else fail "the log has no 'you allowed it' line for the terminal answer"; fi
  if clusia agent log "$PR" | grep -q '^  [✓⊘] '; then pass "agent log prints the permission lines"; else fail "agent log prints no permission lines"; fi
}

step_end() {
  say "10. The end"
  echo "  This discards the review of $PR on this Mac (its draft items are lost)."
  echo "  The clusia-check-$RUN-* files stay untracked in its worktree; this check does not publish."
  if ! yes_or_no "Discard it now?"; then
    echo "  Skipped: the review was kept, with its draft. Discard it later with: clusia review discard $PR"
    return
  fi
  if clusia review discard "$PR" >/dev/null 2>"$ERR"; then
    pass "the review was discarded"
  else
    fail "the review was not discarded: $(cat "$ERR")"
  fi
  if log_has "(you allowed it)"; then pass "the agent log is kept, permission lines included"; else fail "the agent log is gone"; fi
  echo "  The rules you granted died with the review. Opening it again starts clean."
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
step_once
step_review
step_deny
step_expire
step_slow
step_notification
step_sandbox
step_terminal
step_end
summary
