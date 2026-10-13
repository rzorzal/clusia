#!/usr/bin/env bash
# Owner check for Codex as the second harness: your own Codex login, on a pull request of yours
# in a repository you do not mind pushing test commits to, with the window in front of you.
# It spends some of your Codex usage (the chat, two checks and one revalidation round), opens a
# real review, and asks you to do the human parts yourself: answer permission requests, add a
# comment, push a commit and discard the review. The script pushes and publishes nothing, and
# it never answers a permission request.
#
#   scripts/sp2-m5-check.sh --pr rzorzal/clusia#123     run the check (asks first)
#   scripts/sp2-m5-check.sh --list                      print the steps and exit
#   add --yes to skip the first question below; the script still asks on the terminal before it
#   changes your settings, and every step's own question is still yours to answer (a question
#   without an answer counts as no)
#
# Needs Clusia installed (clusia on the PATH, signed in to GitHub), Codex 0.162.0 or newer
# installed and signed in (codex login), and a branch of the pull request you can push to. Run it
# on a machine where Codex is allowed: some work machines delete the codex program, and the check
# stops at the first step when `codex --version` fails. The window is started for you; look at it
# when a step says so. If the pull request already has a review of yours on this Mac, its draft is
# part of the check: use a pull request you do not mind opening fresh.
set -u

PR=""
LIST=0
ASSUME_YES=0
passed=0
failed=0
ERR="$(mktemp -t sp2-m5-check.XXXXXX)"
OLD_KIND=""
OLD_PROGRAM=""
PROGRAM_READ=0
SCHEMA=""
OLD_REVALIDATE=""
OLD_QUIET=""
OLD_ON_OPEN=""
OLD_SECURITY=""
OLD_AUDIT=""
CHANGED=0
APP_PID=""
BASELINE_SERVERS=0
# Set to wait at most this many seconds at every step, instead of each step's own time.
WAIT_CAP="${SP2_CHECK_WAIT:-}"

restore_settings() {
  # The check changes seven settings; the owner's own values come back when it ends. The kind
  # goes back first: a change of kind clears the program, so the program is written after it.
  [ "$CHANGED" = 1 ] || return 0
  [ -n "$OLD_KIND" ] && clusia config set harness.kind "$OLD_KIND" >/dev/null 2>&1
  [ "$PROGRAM_READ" = 1 ] && clusia config set harness.program "$OLD_PROGRAM" >/dev/null 2>&1
  [ -n "$OLD_REVALIDATE" ] && clusia config set harness.revalidate "$OLD_REVALIDATE" >/dev/null 2>&1
  [ -n "$OLD_QUIET" ] && clusia config set harness.revalidate_quiet_secs "$OLD_QUIET" >/dev/null 2>&1
  [ -n "$OLD_ON_OPEN" ] && clusia config set harness.on_open "$OLD_ON_OPEN" >/dev/null 2>&1
  [ -n "$OLD_SECURITY" ] && clusia config set harness.check_security "$OLD_SECURITY" >/dev/null 2>&1
  [ -n "$OLD_AUDIT" ] && clusia config set harness.audit "$OLD_AUDIT" >/dev/null 2>&1
  [ "$PROGRAM_READ" = 1 ] || echo "Could not read the old harness.program; check Config › Harness: Program." >&2
  [ -n "$OLD_KIND" ] || echo "Could not read the old harness.kind; check Config › Harness: which agent is selected." >&2
  [ -n "$OLD_REVALIDATE" ] || echo "Could not read the old harness.revalidate; check Config › Harness: Revalidate my draft." >&2
  [ -n "$OLD_QUIET" ] || echo "Could not read the old harness.revalidate_quiet_secs; check Config › Harness: Wait for pushes to settle." >&2
  [ -n "$OLD_ON_OPEN" ] || echo "Could not read the old harness.on_open; check Config › Harness: When I open a review." >&2
  [ -n "$OLD_SECURITY" ] || echo "Could not read the old harness.check_security; check Config › Harness: Check security." >&2
  [ -n "$OLD_AUDIT" ] || echo "Could not read the old harness.audit; check Config › Harness: Audit." >&2
  return 0
}
close_window() {
  if [ -n "$APP_PID" ]; then kill "$APP_PID" 2>/dev/null; fi
}
drop_schema() {
  if [ -n "$SCHEMA" ]; then find "$SCHEMA" -delete 2>/dev/null; fi
}
trap 'close_window; restore_settings; drop_schema; rm -f "$ERR" "$ERR.out" "$ERR.out.sec" "$ERR.out.audit" "$ERR.out.more"' EXIT
# clusia exits 130 on a Ctrl-C by status, not by the signal, so bash would go on to the next
# step; the script ends instead (the EXIT trap still cleans up).
trap 'exit 130' INT TERM HUP

STEPS=(
  "preflight: codex and clusia are installed, codex --version and codex login status answer, the app-server schema has every name Clúsia reads, and Codex is selected"
  "switch: Config › Harness shows Codex selected and Test answers with the version and your login"
  "open: the review opens and the status pill and the Agent panel say Codex"
  "chat: a question streams an answer, a second message works in the same process, and the chat goes on after you close and reopen the review"
  "isolation: your Codex hooks and MCP servers stay out of Clúsia's threads"
  "approve: a command Codex wants to run asks you, and Deny, Allow once and the timeout each behave"
  "checks: Security and Audit run on Codex, and the chat stays free meanwhile"
  "revalidate: you save the review, push a commit, and Codex revalidates the draft"
  "end: you discard the review and no codex app-server is left running"
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

# Like ask, but an `s` answer means the question does not apply to this machine: nothing is counted.
ask_if_applicable() {
  local answer
  read -r -p "  ? $1 [y/n/s=does not apply] " answer
  case "$answer" in
    y | Y | yes) pass "$1" ;;
    s | S) printf '  SKIP  %s\n' "$1" ;;
    *) fail "$1" ;;
  esac
}

# The names Clúsia reads or writes in the app-server's messages. The schema Codex generates for
# this install must contain each one; a name that is missing means the codec and this Codex
# disagree, and the step that uses it would fail or, for the thread config, fail open.
SCHEMA_NAMES=(
  "thread/start" "thread/resume" "thread/fork" "turn/start" "turn/interrupt"
  "turn/started" "turn/completed" "item/started" "item/completed" "item/agentMessage/delta"
  "item/commandExecution/requestApproval" "item/fileChange/requestApproval"
  "approvalPolicy" "approvalsReviewer" "developerInstructions" "sandbox" "threadId" "turnId"
  "grantRoot" "willRetry" "changes"
)

schema_has() { grep -rqF -- "$1" "$SCHEMA"; }

# The thread-start parameters must offer a config override: it carries the hooks-off and the
# empty MCP table.
thread_config_is_offered() {
  local file
  for file in $(grep -rlF ThreadStartParams "$SCHEMA" 2>/dev/null); do
    grep -qF '"config"' "$file" && return 0
  done
  return 1
}

check_schema() {
  local name
  SCHEMA="$(mktemp -d -t sp2-m5-schema.XXXXXX)"
  if ! codex app-server generate-json-schema --out "$SCHEMA" >/dev/null 2>"$ERR"; then
    fail "codex app-server generate-json-schema ran: $(cat "$ERR")"
    return 0
  fi
  pass "codex app-server generate-json-schema ran"
  for name in "${SCHEMA_NAMES[@]}"; do
    if schema_has "$name"; then pass "the schema has $name"; else fail "the schema has $name"; fi
  done
  if thread_config_is_offered; then pass "ThreadStartParams offers a config override"; else fail "ThreadStartParams offers a config override"; fi
}

usage() {
  echo "usage: $0 [--yes] [--list | --pr owner/repo#N]" >&2
}

confirm() {
  local answer
  [ "$ASSUME_YES" = 1 ] && return 0
  echo "This opens a real review of $PR and asks YOUR Codex (it uses your plan) to chat, check"
  echo "security, audit and revalidate your draft. You must answer every request for a command"
  echo "yourself: the script never answers for you. It changes seven Config › Harness settings for"
  echo "the run and puts them back. It pushes nothing: you push the test commit to the pull"
  echo "request's branch, and you discard the review at the end (you publish nothing)."
  printf 'Continue? [y/N] '
  read -r answer || answer=n
  case "$answer" in y | Y | yes) return 0 ;; *) return 1 ;; esac
}

app_servers() { pgrep -f 'codex app-server' 2>/dev/null | wc -l | tr -d ' '; }
# A function, so each poll of wait_for counts again.
no_new_servers() { [ "$(app_servers)" -le "$BASELINE_SERVERS" ]; }
chat_said_42() { grep -Eqx '[[:space:]]*42[.]?[[:space:]]*' "$ERR.out"; }
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

step_preflight() {
  local version
  say "1. Preflight"
  if ! version="$(codex --version 2>&1)"; then
    echo "  codex --version failed, so Codex cannot run on this machine: $version"
    echo "  Run this check where Codex is allowed. Nothing was changed."
    exit 1
  fi
  pass "codex --version answers: $version"
  check "Codex is signed in (codex login status)" codex login status
  check_schema
  check "clusia is installed" command -v clusia
  check "the daemon answers (it is started if needed)" clusia daemon start
  BASELINE_SERVERS="$(app_servers)"
  OLD_KIND="$(clusia config get harness.kind 2>/dev/null)"
  if OLD_PROGRAM="$(clusia config get harness.program 2>/dev/null)"; then PROGRAM_READ=1; fi
  OLD_REVALIDATE="$(clusia config get harness.revalidate 2>/dev/null)"
  OLD_QUIET="$(clusia config get harness.revalidate_quiet_secs 2>/dev/null)"
  OLD_ON_OPEN="$(clusia config get harness.on_open 2>/dev/null)"
  OLD_SECURITY="$(clusia config get harness.check_security 2>/dev/null)"
  OLD_AUDIT="$(clusia config get harness.audit 2>/dev/null)"
  echo "  The check selects Codex (which clears the program path, as any change of agent does), turns revalidation on with a 60 second wait and turns the three"
  echo "  'When I open a review' boxes off, so the only Codex turns it spends are the ones you see"
  echo "  here. Your values (agent '$OLD_KIND', program '$OLD_PROGRAM', revalidate '$OLD_REVALIDATE', wait '$OLD_QUIET' s, on"
  echo "  open '$OLD_ON_OPEN', security '$OLD_SECURITY', audit '$OLD_AUDIT') are put back when it ends."
  yes_or_no "Change those seven settings now (they are put back when the check ends)?" || {
    echo "Nothing was changed."
    exit 1
  }
  CHANGED=1
  clusia config set harness.kind codex >/dev/null 2>&1
  clusia config set harness.program "" >/dev/null 2>&1
  clusia config set harness.revalidate true >/dev/null 2>&1
  clusia config set harness.revalidate_quiet_secs 60 >/dev/null 2>&1
  clusia config set harness.on_open wait >/dev/null 2>&1
  clusia config set harness.check_security false >/dev/null 2>&1
  clusia config set harness.audit false >/dev/null 2>&1
  check "Codex is the selected agent" test "$(clusia config get harness.kind 2>/dev/null)" = "codex"
  check "no program path is left over from the other agent" test -z "$(clusia config get harness.program 2>/dev/null)"
  check "revalidation is on" test "$(clusia config get harness.revalidate 2>/dev/null)" = "true"
  check "$PR is a pull request clusia can open" clusia open "$PR"
  start_window
}

step_switch() {
  say "2. Config › Harness"
  echo "  In the window open Config › Harness."
  ask "Is the Codex card selected, reading 'codex app-server, with your login', with no Claude Code settings switch and a locked sandbox row?"
  echo "  Press Test."
  ask "Does it read 'Codex 0.16… answered in … s' with your login after a dot?"
}

step_open() {
  say "3. Open the review"
  echo "  In the window open the review of $PR (it is already open if the window started on it)."
  ask "Does the status bar say 'Codex · …' and the Agent panel header say Codex, never Claude Code?"
}

step_chat() {
  say "4. Chat"
  clusia ask "$PR" "What is 6 times 7? Answer with just the number." </dev/null >"$ERR.out" 2>"$ERR" &
  wait_for "the chat answered (it said 42)" 300 chat_said_42
  clusia ask "$PR" "Now add one to that number. Answer with just the number." </dev/null >"$ERR.out.more" 2>"$ERR"
  if grep -Eqx '[[:space:]]*43[.]?[[:space:]]*' "$ERR.out.more"; then pass "a second message in the same process was answered (43)"; else fail "the second message was not answered with 43: $(cat "$ERR.out.more" "$ERR")"; fi
  check "one codex app-server serves the review, not one per message" test "$(app_servers)" -le "$((BASELINE_SERVERS + 1))"
  ask "Did each answer appear once in the chat, with no repeated or doubled text?"
  echo "  Now close the review's tab (Keep for later) and open it again from Home."
  ask "Does the chat still show that question and its answer, and does the panel say the session resumed?"
  check "agent log prints the chat" log_has "6 times 7"
}

step_isolation() {
  say "5. Hooks and MCP servers stay out"
  clusia ask "$PR" "List every tool you can call that is not a plain shell or file tool, one per line, or answer NONE if there are none." </dev/null >"$ERR.out.more" 2>"$ERR"
  cat "$ERR.out.more"
  if grep -qiE '(^|[^A-Za-z])none([^A-Za-z]|$)' "$ERR.out.more"; then pass "the agent listed no tools beyond the shell and files"; else fail "the agent listed extra tools, so your MCP servers may be loading"; fi
  ask_if_applicable "Do you have a Codex MCP server in your own config? If so, did the answer above leave it out?"
  ask_if_applicable "Do you have a Codex hook (for example a PermissionRequest or PreToolUse hook that writes a file) in your own config? If so, did it stay silent during this chat?"
}

step_approve() {
  say "6. A command asks you"
  clusia ask "$PR" "Run \`git status --short\` in the worktree and tell me what it printed." </dev/null >"$ERR.out" 2>"$ERR" &
  echo "  The window shows 'Codex wants to run a command'. Choose Deny once, and watch the chat go on."
  ask "Did Deny stop the command, with the chat saying it was denied?"
  clusia ask "$PR" "Now run \`git status --short\` again." </dev/null >"$ERR.out" 2>"$ERR" &
  echo "  Choose Allow once this time."
  ask "Did the command run and the chat show what it printed?"
  clusia ask "$PR" "Run \`git log -1 --oneline\`." </dev/null >"$ERR.out" 2>"$ERR" &
  echo "  This time do not answer. The request is denied when the time in Config › Harness passes."
  ask "Was the request denied by itself, with a notification and a line in the chat?"
}

step_checks() {
  local status
  say "7. Security and Audit"
  clusia ask "$PR" "What is 6 times 7? Answer with just the number." </dev/null >"$ERR.out" 2>"$ERR" &
  clusia check "$PR" --security >"$ERR.out.sec" 2>"$ERR"
  status=$?
  cat "$ERR.out.sec"
  if [ "$status" -eq 0 ]; then pass "clusia check --security finished (exit 0)"; else fail "clusia check --security failed with exit $status: $(cat "$ERR")"; fi
  clusia check "$PR" --audit >"$ERR.out.audit" 2>"$ERR"
  status=$?
  cat "$ERR.out.audit"
  if [ "$status" -eq 0 ]; then pass "clusia check --audit finished (exit 0)"; else fail "clusia check --audit failed with exit $status: $(cat "$ERR")"; fi
  ask "Did the Security and Audits tabs show results that say Codex, and did the chat answer while they ran?"
}

step_revalidate() {
  say "8. Revalidation"
  echo "  In the window add one comment on a line of the Diff, close the review's tab and choose"
  echo "  Keep for later. Then, in a clone of the pull request's branch, push one commit that changes"
  echo "  the commented code (you push, not this script). Wait about two minutes."
  ask "Did exactly one notification arrive, saying the review was revalidated?"
  ask "Does the revalidation screen say Codex checked your comment against the new commit, with a verdict row for it?"
  clusia revalidate "$PR" --again </dev/null >"$ERR.out" 2>"$ERR"
  local status=$?
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
  wait_for "no codex app-server is left running for it" 15 no_new_servers
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
