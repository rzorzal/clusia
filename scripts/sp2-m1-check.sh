#!/usr/bin/env bash
# Owner check for the agent chat: your own Claude Code, on a real pull request of yours, with the
# window in front of you. It spends a little of your Claude Code usage, opens a real review of
# the pull request, and discards that review at the end. Nothing is published to GitHub.
#
#   scripts/sp2-m1-check.sh --pr rzorzal/clusia#123     run the check (asks first)
#   scripts/sp2-m1-check.sh --list                      print the steps and exit
#   add --yes to skip the question below, for a run without a terminal
#
# Needs Clusia installed (clusia on the PATH, signed in to GitHub) and Claude Code installed and
# signed in. The window is started for you; look at it when a step says so.
set -u

PR=""
LIST=0
ASSUME_YES=0
passed=0
failed=0
ERR="$(mktemp -t sp2-m1-check.XXXXXX)"
OLD_ON_OPEN=""
restore_settings() {
  # The check turns the summary on; the owner's own choice comes back when it ends.
  if [ -n "$OLD_ON_OPEN" ]; then clusia config set harness.on_open "$OLD_ON_OPEN" >/dev/null 2>&1; fi
}
trap 'restore_settings; rm -f "$ERR" "$ERR.out"' EXIT

STEPS=(
  "preflight: claude and clusia are installed, and the daemon answers"
  "summary: opening the review starts the agent and its summary shows in the chat"
  "question: a question makes the agent read files, and the window shows the file reads"
  "suggestion: Accept into draft puts a suggested comment in the draft"
  "resume: closing and reopening the review continues the same session"
  "denied: a request that needs a command is denied with a visible line"
  "terminal: clusia ask, agent log and agent stop work in a terminal"
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
  local what="$1" secs="$2" i=0
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
  echo "This opens a real review of $PR, sends several questions to YOUR Claude Code (it uses"
  echo "your plan), asks you to look at the window, and discards the review at the end."
  echo "It publishes nothing to GitHub."
  printf 'Continue? [y/N] '
  read -r answer || answer=n
  case "$answer" in y | Y | yes) return 0 ;; *) return 1 ;; esac
}

log_has() { clusia --json agent log "$PR" 2>/dev/null | grep -q "$1"; }
has_text() { log_has '"type":"text"'; }
has_suggestion_text() { grep -q 'Suggested comments:' "$ERR.out"; }
agent_items() { clusia --json review status "$PR" 2>/dev/null | grep -io '"origin":"agent"' | wc -l | tr -d ' '; }

app_binary() {
  local dir
  for dir in /Applications "$HOME/Applications"; do
    if [ -x "$dir/Clusia.app/Contents/MacOS/clusia-app" ]; then
      echo "$dir/Clusia.app/Contents/MacOS/clusia-app"
      return 0
    fi
  done
  return 1
}

step_preflight() {
  say "1. Preflight"
  check "Claude Code is installed" command -v claude
  check "Claude Code answers" claude --version
  check "clusia is installed" command -v clusia
  check "the daemon answers (it is started if needed)" clusia daemon start
  check "$PR is a pull request clusia can open" clusia open "$PR"
  OLD_ON_OPEN="$(clusia config get harness.on_open 2>/dev/null)"
  echo "  harness.on_open is '$OLD_ON_OPEN'; it is set to summarize for this check and put back when it ends."
  clusia config set harness.on_open summarize >/dev/null 2>&1
  local app
  if app="$(app_binary)"; then
    "$app" --review "$PR" >/dev/null 2>&1 &
    pass "the window was started on $PR"
  else
    fail "Clusia.app is installed (the window could not be started)"
  fi
}

step_summary() {
  say "2. The summary"
  echo "  The agent starts by itself and summarizes the pull request."
  wait_for "the agent wrote its summary (it is in the log)" 180 has_text
  ask "Does the Agent tab of the window show that summary (the tab is selected when the agent speaks)?"
}

step_question() {
  say "3. A question"
  clusia ask "$PR" "Open Cargo.toml and tell me the names of the workspace members. Be brief." >"$ERR.out" 2>"$ERR"
  check "the answer is not empty" test -s "$ERR.out"
  if grep -q '✓ Read' "$ERR"; then pass "clusia ask printed the file reads"; else fail "clusia ask printed no ✓ Read line"; fi
  ask "Did the window show the question's file reads (✓ Read …) and its answer?"
}

step_suggestion() {
  say "4. A suggested comment"
  local before after
  before="$(agent_items)"
  clusia ask "$PR" "Suggest exactly one review comment on the line you find most questionable, as a clusia-suggestion block." >"$ERR.out" 2>"$ERR"
  if has_suggestion_text; then
    pass "the agent made a suggestion (clusia ask listed it)"
  else
    fail "the agent made no suggestion; run the step again"
  fi
  echo "  In the window, click Accept into draft on the new card."
  ask "Did the card become '✓ Added to your draft' and did the draft count go up?"
  after="$(agent_items)"
  if [ "${after:-0}" -gt "${before:-0}" ]; then
    pass "the draft has a comment from the agent"
  else
    fail "the draft has no new comment from the agent"
  fi
}

step_resume() {
  say "5. Closing and reopening"
  echo "  Close the review tab (or the window), then open the review again from Home."
  ask "Did the chat come back with the earlier questions and answers?"
  clusia ask "$PR" "In one short sentence: what was my first question in this session?" >"$ERR.out" 2>"$ERR"
  if [ -s "$ERR.out" ]; then pass "the agent answered after the reopen"; else fail "no answer after the reopen"; fi
  ask "Did that answer remember the earlier questions (the session continued)?"
}

step_denied() {
  say "6. A command is denied"
  clusia ask "$PR" "Run the shell command 'cargo --version' and tell me its output." >"$ERR.out" 2>"$ERR"
  if grep -q '⊘ wanted to use' "$ERR"; then
    pass "clusia ask printed the denied line"
  else
    fail "no denied line was printed (did the agent run the command?)"
  fi
  ask "Did the window show a '⊘ Wanted to run …' line for it?"
}

step_terminal() {
  say "7. The terminal"
  if clusia agent log "$PR" | grep -q '^you: '; then pass "agent log prints the chat"; else fail "agent log prints no question"; fi
  check "agent stop answers while nothing runs" clusia agent stop "$PR"
}

step_end() {
  say "8. The end"
  echo "  This discards the review of $PR on this Mac (its draft items are lost)."
  echo "  Publishing a review ends the session the same way; this check does not publish."
  if ! yes_or_no "Discard it now?"; then
    echo "  Skipped: the review was kept, with its draft. Discard it later with: clusia review discard $PR"
    return
  fi
  clusia review discard "$PR" >/dev/null 2>&1
  if clusia review status 2>/dev/null | grep -q "$PR"; then fail "the review is gone"; else pass "the review is gone"; fi
  if log_has '"type":"user"'; then pass "the agent log is kept"; else fail "the agent log is gone"; fi
  echo "  Opening the pull request again starts a new session with a new summary."
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
step_summary
step_question
step_suggestion
step_resume
step_denied
step_terminal
step_end
summary
