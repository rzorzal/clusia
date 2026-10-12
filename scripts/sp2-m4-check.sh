#!/usr/bin/env bash
# Owner check for the revalidation of saved reviews: your own Claude Code, on a pull request of
# yours in a repository you do not mind pushing test commits to, with the window in front of you.
# It spends some of your Claude Code usage (one turn per round of pushes), opens a real review,
# and asks you to do three things yourself: add comments in the window, push commits, and publish
# the review. The script pushes and publishes nothing, and it never answers a permission request.
#
#   scripts/sp2-m4-check.sh --pr rzorzal/clusia#123     run the check (asks first)
#   scripts/sp2-m4-check.sh --list                      print the steps and exit
#   add --yes to skip the first question below; the script still asks on the terminal before it
#   changes your settings and before it runs Claude Code from the terminal, and every step's
#   own question is still yours to answer (a question without an answer counts as no)
#
# Needs Clusia installed (clusia on the PATH, signed in to GitHub), Claude Code installed and
# signed in, and a branch of the pull request you can push to. The window is started for you; look
# at it when a step says so. If the pull request already has a review of yours on this Mac, its
# draft is part of the check: use a pull request you do not mind opening fresh.
set -u

PR=""
LIST=0
ASSUME_YES=0
passed=0
failed=0
ERR="$(mktemp -t sp2-m4-check.XXXXXX)"
OLD_REVALIDATE=""
OLD_QUIET=""
OLD_ON_OPEN=""
OLD_SECURITY=""
OLD_AUDIT=""
CHANGED=0
APP_PID=""
# Set to wait at most this many seconds at every step, instead of each step's own time.
WAIT_CAP="${SP2_CHECK_WAIT:-}"

restore_settings() {
  # The check changes five settings; the owner's own values come back when it ends.
  [ "$CHANGED" = 1 ] || return 0
  [ -n "$OLD_REVALIDATE" ] && clusia config set harness.revalidate "$OLD_REVALIDATE" >/dev/null 2>&1
  [ -n "$OLD_QUIET" ] && clusia config set harness.revalidate_quiet_secs "$OLD_QUIET" >/dev/null 2>&1
  [ -n "$OLD_ON_OPEN" ] && clusia config set harness.on_open "$OLD_ON_OPEN" >/dev/null 2>&1
  [ -n "$OLD_SECURITY" ] && clusia config set harness.check_security "$OLD_SECURITY" >/dev/null 2>&1
  [ -n "$OLD_AUDIT" ] && clusia config set harness.audit "$OLD_AUDIT" >/dev/null 2>&1
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
trap 'close_window; restore_settings; rm -f "$ERR" "$ERR.out"' EXIT
# clusia exits 130 on a Ctrl-C by status, not by the signal, so bash would go on to the next
# step; the script ends instead (the EXIT trap still cleans up).
trap 'exit 130' INT TERM HUP

STEPS=(
  "preflight: claude and clusia are installed, the daemon answers and revalidation is on with a one-minute wait"
  "save: you add four comments in the window and keep the review for later"
  "push: you push commits that keep, move, remove and change the commented code"
  "notify: exactly one notification arrives, saying the review was revalidated"
  "screen: the revalidation screen shows a verdict for every comment and changes nothing yet"
  "cli: clusia revalidate --again runs the round again and prints one line per comment"
  "decide: Put it back, Keep the move, Use new wording, Remove comment and Accept all work"
  "end: you publish the review, GitHub raises no error, and the review is gone"
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
  echo "This opens a real review of $PR and asks YOUR Claude Code (it uses your plan) to check your"
  echo "draft comments against the commits you push. You must answer every request for a command"
  echo "yourself: the script never answers for you. It changes five Config › Harness settings for the"
  echo "run and puts them back. It pushes nothing: you push the test commits to the pull request's"
  echo "branch, and you publish the review at the end (a real review comment on $PR)."
  printf 'Continue? [y/N] '
  read -r answer || answer=n
  case "$answer" in y | Y | yes) return 0 ;; *) return 1 ;; esac
}

# Draft items you wrote, as the terminal prints them in JSON.
human_items() { clusia --json review status "$PR" 2>/dev/null | grep -Eio '"origin": ?"human"' | wc -l | tr -d ' '; }
# A function, so each poll of wait_for counts again.
four_comments() { [ "$(human_items)" -ge 4 ]; }

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
  OLD_REVALIDATE="$(clusia config get harness.revalidate 2>/dev/null)"
  OLD_QUIET="$(clusia config get harness.revalidate_quiet_secs 2>/dev/null)"
  OLD_ON_OPEN="$(clusia config get harness.on_open 2>/dev/null)"
  OLD_SECURITY="$(clusia config get harness.check_security 2>/dev/null)"
  OLD_AUDIT="$(clusia config get harness.audit 2>/dev/null)"
  echo "  The check turns revalidation on, shortens the wait for pushes to settle to 60 seconds and"
  echo "  turns the three 'When I open a review' boxes off, so the only Claude Code turn it spends"
  echo "  is the revalidation. Your values (revalidate '$OLD_REVALIDATE', wait '$OLD_QUIET' s, on open"
  echo "  '$OLD_ON_OPEN', security '$OLD_SECURITY', audit '$OLD_AUDIT') are put back when it ends."
  yes_or_no "Change those five settings now (they are put back when the check ends)?" || {
    echo "Nothing was changed."
    exit 1
  }
  CHANGED=1
  clusia config set harness.revalidate true >/dev/null 2>&1
  clusia config set harness.revalidate_quiet_secs 60 >/dev/null 2>&1
  clusia config set harness.on_open wait >/dev/null 2>&1
  clusia config set harness.check_security false >/dev/null 2>&1
  clusia config set harness.audit false >/dev/null 2>&1
  check "revalidation is on" test "$(clusia config get harness.revalidate 2>/dev/null)" = "true"
  check "the wait is 60 seconds" test "$(clusia config get harness.revalidate_quiet_secs 2>/dev/null)" = "60"
  check "$PR is a pull request clusia can open" clusia open "$PR"
  start_window
}

step_save() {
  say "2. Save a review with comments"
  echo "  In the window, in the Diff of $PR, add four line comments (click a line number):"
  echo "    a. on code you will leave alone,"
  echo "    b. on code you will push other lines above (it will move down),"
  echo "    c. on code you will delete,"
  echo "    d. on code whose meaning you will change."
  wait_for "four comments of yours are in the draft" 900 four_comments
  echo "  Now close the review's tab and choose Keep for later. The window can stay open."
  ask "Is the review saved, with its four comments still in the draft?"
}

step_push() {
  say "3. Push commits"
  echo "  In a clone of the pull request's branch, make and push (you push, not this script):"
  echo "    1. a commit that adds a few lines above the code of comment b,"
  echo "    2. a commit that deletes the code of comment c,"
  echo "    3. a commit that changes what the code of comment d does."
  echo "  Push them together, one after the other, in less than a minute."
  ask "Did you push the commits to the pull request's branch?"
}

step_notify() {
  say "4. One notification"
  echo "  Clusia checks for new commits every minute and waits 60 seconds after the last push, then"
  echo "  asks Claude Code. Give it up to about four minutes, and do not touch the review window."
  ask "Did exactly one notification arrive, reading 'Review #… revalidated: …' with counts?"
  ask "Was there no second notification saying the review has new commits?"
}

step_screen() {
  say "5. The revalidation screen"
  echo "  Open the review from the notification (or the Home list)."
  ask "Does the screen say Claude Code checked your comments against the new commits, never 'verified'?"
  ask "Does the banner list the new commits you pushed (short sha and title)?"
  ask "Does every comment have a row (Kept, Moved, Obsolete, New wording or Not checked), and does the comment you deleted code for read Obsolete?"
  ask "Is every draft comment still exactly as you wrote it (nothing changed before you clicked)?"
}

step_cli() {
  local status
  say "6. clusia revalidate"
  echo "  This runs the revalidation again at the terminal (--again) and prints a line for each"
  echo "  comment. A rerun replaces the proposals, so it comes before you decide anything."
  yes_or_no "Run clusia revalidate --again now? It spends one Claude Code turn." || {
    echo "  Skipped: clusia revalidate was not run."
    return 0
  }
  clusia revalidate "$PR" --again </dev/null >"$ERR.out" 2>"$ERR"
  status=$?
  cat "$ERR.out"
  if [ "$status" -eq 0 ]; then pass "clusia revalidate finished (exit 0)"; else fail "clusia revalidate failed with exit $status: $(cat "$ERR")"; fi
  if grep -Eq '^(KEPT|MOVED|OBSOLETE|REWORD|NOT CHECKED)  ' "$ERR.out"; then
    pass "it printed lines as VERDICT  file:line  text"
  else
    fail "its output has no VERDICT line"
  fi
  check "a bad option is a usage error (exit 2)" sh -c 'clusia revalidate "$0" --nope >/dev/null 2>&1 </dev/null; test $? -eq 2' "$PR"
}

step_decide() {
  say "7. Decide"
  echo "  In the window, open the review again if it shows the tabs. On a Moved row click Put it back"
  echo "  (if it is enabled), on another click Keep the move; on a New wording row click Use new"
  echo "  wording; on an Obsolete row click Remove comment. Then press Back to review: the compact"
  echo "  strip should count what is left."
  ask "Did each click do what its button says, and did the removed comment stay in the draft marked obsolete?"
  echo "  Click Accept all on what is left (or Review them first, then Accept all)."
  ask "Are there no suggestions left, and is every remaining comment on a line of the new diff?"
}

step_end() {
  say "8. Publish"
  echo "  In the window press Finalize review, choose Comment and publish. This posts a real review"
  echo "  on $PR with what is left of the draft (a comment you removed is left out, not deleted);"
  echo "  you publish, this script does not."
  ask "Did the publish finish with no GitHub error (no anchor refused)?"
  if clusia --json review status "$PR" >/dev/null 2>&1; then
    echo "  Skipped: the review was kept, with its draft. Discard it later with: clusia review discard $PR"
  else
    pass "the review is gone after publishing"
  fi
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
step_save
step_push
step_notify
step_screen
step_cli
step_decide
step_end
summary
