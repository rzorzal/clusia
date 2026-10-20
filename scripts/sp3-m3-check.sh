#!/usr/bin/env bash
# Owner check for the Diff's Story and Calls views: your own agent, on a pull request of yours
# (a demo one on rzorzal/* with Rust, TypeScript and Python files is best), with the window in
# front of you. It spends some of your agent usage (one turn to write the story and one to rewrite
# it), opens a real review of the pull request, and discards that review at the end. It changes
# six Config › Harness settings for the run and puts them back. It publishes nothing to GitHub.
# You push one small commit to the pull request's branch yourself in step 6; the script pushes
# nothing. Clusia asks you before the agent runs a command, as always, and the script never
# answers for you.
#
#   scripts/sp3-m3-check.sh --pr rzorzal/clusia#123     run the check (asks first)
#   scripts/sp3-m3-check.sh --list                      print the steps and exit
#   add --yes to skip the question below; every step still asks you on the terminal, and
#   without one each of those questions is answered no
#
# Needs Clusia installed (clusia on the PATH, signed in to GitHub) and an agent set up in
# Config › Harness. The window is started for you; look at it when a step says so. If the pull
# request already has a review of yours on this Mac, its draft is lost at the end: use a pull
# request you do not mind opening fresh.
set -u

PR=""
LIST=0
ASSUME_YES=0
passed=0
failed=0
ERR="$(mktemp -t sp3-m3-check.XXXXXX)" || {
  echo "Cannot create a temp file." >&2
  exit 1
}
OLD_ON_OPEN=""
OLD_SECURITY=""
OLD_AUDIT=""
OLD_DIAGRAMS=""
OLD_KEYWORDS=""
OLD_STORY=""
CHANGED=0
BEFORE_ITEMS=0
HEAD_BEFORE=""
APP_PID=""
STORY_PID=""
# Set to wait at most this many seconds at every step, instead of each step's own time.
WAIT_CAP="${SP3_CHECK_WAIT:-}"

restore_settings() {
  # The check changes six settings; the owner's own values come back when it ends.
  [ "$CHANGED" = 1 ] || return 0
  [ -n "$OLD_ON_OPEN" ] && clusia config set harness.on_open "$OLD_ON_OPEN" >/dev/null 2>&1
  [ -n "$OLD_SECURITY" ] && clusia config set harness.check_security "$OLD_SECURITY" >/dev/null 2>&1
  [ -n "$OLD_AUDIT" ] && clusia config set harness.audit "$OLD_AUDIT" >/dev/null 2>&1
  [ -n "$OLD_DIAGRAMS" ] && clusia config set harness.diagrams "$OLD_DIAGRAMS" >/dev/null 2>&1
  [ -n "$OLD_KEYWORDS" ] && clusia config set harness.keywords_3d "$OLD_KEYWORDS" >/dev/null 2>&1
  [ -n "$OLD_STORY" ] && clusia config set harness.story "$OLD_STORY" >/dev/null 2>&1
  [ -n "$OLD_ON_OPEN" ] || echo "Could not read the old harness.on_open; check Config › Harness: When I open a review." >&2
  [ -n "$OLD_SECURITY" ] || echo "Could not read the old harness.check_security; check Config › Harness: Check security." >&2
  [ -n "$OLD_AUDIT" ] || echo "Could not read the old harness.audit; check Config › Harness: Audit the change." >&2
  [ -n "$OLD_DIAGRAMS" ] || echo "Could not read the old harness.diagrams; check Config › Harness: Draw diagrams." >&2
  [ -n "$OLD_KEYWORDS" ] || echo "Could not read the old harness.keywords_3d; check Config › Harness: 3D keyword view." >&2
  [ -n "$OLD_STORY" ] || echo "Could not read the old harness.story; check Config › Harness: Write the story when I open Story." >&2
  return 0
}
# Stops a story the script started, so nothing keeps spending after the check ends.
# A job started in the background of a script may ignore Ctrl-C, so the stop escalates.
stop_story() {
  local i
  if [ -n "$STORY_PID" ]; then
    kill -INT "$STORY_PID" 2>/dev/null
    for i in 1 2 3; do
      kill -0 "$STORY_PID" 2>/dev/null || break
      sleep 1
    done
    if kill -0 "$STORY_PID" 2>/dev/null; then
      kill -TERM "$STORY_PID" 2>/dev/null
      sleep 1
    fi
    if kill -0 "$STORY_PID" 2>/dev/null; then kill -KILL "$STORY_PID" 2>/dev/null; fi
    wait "$STORY_PID" 2>/dev/null
    STORY_PID=""
  fi
}
close_window() {
  if [ -n "$APP_PID" ]; then kill "$APP_PID" 2>/dev/null; fi
}
drop_err() {
  if [ -n "$ERR" ]; then rm -f "$ERR" "$ERR".*; fi
}
trap 'stop_story; close_window; restore_settings; drop_err' EXIT
# clusia exits 130 on a Ctrl-C by status, not by the signal, so bash would go on to the next
# step; the script ends instead (the EXIT trap still cleans up).
trap 'exit 130' INT TERM HUP

STEPS=(
  "preflight: clusia is installed, the daemon answers, the agent is set up, six settings are set for the run and the review opens"
  "static: clusia story lists the static order and clusia calls prints the graph at depth 3 and at depth 1, with no agent involved"
  "views: Story and Calls in the window show the steps, the call graph, the side panel and the expansion, for Rust, TypeScript and Python"
  "write: clusia story --write lets your agent write the story and the Story says who wrote it, from which commit"
  "comment: a comment on a step and a comment on a node arrive in the draft"
  "push: you push a commit and the story is reported as written for an older commit"
  "rewrite: Rewrite makes version 2, and Show changes since version 1 marks what was added, changed and removed"
  "setting: Config opens at Write the story when I open Story"
  "cli: clusia story and clusia calls print their JSON, and a bad option is a usage error"
  "end: discarding the review deletes the story and the call graph"
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
  echo "This opens a real review of $PR and asks YOUR agent (it uses your plan) to write the"
  echo "story of the change (one turn), and once more when you click Rewrite. You must"
  echo "answer every request for a command yourself: the script never answers for you. It"
  echo "changes six Config › Harness settings for the run and puts them back. You push"
  echo "one small commit to the pull request's branch in step 6; the script pushes nothing. It"
  echo "discards the review of $PR on this Mac at the end, so a draft you have there is lost."
  echo "It publishes nothing to GitHub."
  printf 'Continue? [y/N] '
  read -r answer || answer=n
  case "$answer" in y | Y | yes) return 0 ;; *) return 1 ;; esac
}

# Draft items, as the terminal prints them in JSON: one `"body"` each.
draft_items() { clusia --json review status "$PR" 2>/dev/null | grep -Eo '"body": ?"' | wc -l | tr -d ' '; }
# A function, so the count is read again on every poll.
items_grew() { [ "$(draft_items)" -gt "$BEFORE_ITEMS" ]; }

# The commit the pull request is at, as the Story reports it.
story_head() { clusia --json story "$PR" 2>/dev/null | grep -Eo '"head": ?"[0-9a-f]+"' | head -n 1; }
head_moved() {
  clusia open "$PR" >/dev/null 2>&1
  [ -n "$HEAD_BEFORE" ] && [ "$(story_head)" != "$HEAD_BEFORE" ]
}
version_is() { clusia --json story "$PR" 2>/dev/null | grep -Eq "\"number\": ?$1[,}]"; }

# Writes the story without a keyboard, so a request for a command waits for the window and the
# script can never answer it. `wait_story` ends it.
write_detached() {
  clusia story "$PR" --write </dev/null >"$ERR.out" 2>"$ERR" &
  STORY_PID=$!
}
wait_story() {
  local status=0
  if [ -n "$STORY_PID" ]; then
    wait "$STORY_PID" 2>/dev/null
    status=$?
    STORY_PID=""
  fi
  return "$status"
}
story_ended() { ! kill -0 "$STORY_PID" 2>/dev/null; }

app_binary() {
  local dir
  # Set to run another window program instead of the installed one.
  if [ -n "${SP3_CHECK_APP:-}" ]; then
    [ -x "$SP3_CHECK_APP" ] && echo "$SP3_CHECK_APP"
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
  check "clusia is installed" command -v clusia
  check "the daemon answers (it is started if needed)" clusia daemon start
  OLD_ON_OPEN="$(clusia config get harness.on_open 2>/dev/null)"
  OLD_SECURITY="$(clusia config get harness.check_security 2>/dev/null)"
  OLD_AUDIT="$(clusia config get harness.audit 2>/dev/null)"
  OLD_DIAGRAMS="$(clusia config get harness.diagrams 2>/dev/null)"
  OLD_KEYWORDS="$(clusia config get harness.keywords_3d 2>/dev/null)"
  OLD_STORY="$(clusia config get harness.story 2>/dev/null)"
  echo "  The check turns the four boxes of 'When I open a review', the 3D keyword view and 'Write the"
  echo "  story when I open Story' off, so the only turns it runs are the ones you see here. Your values"
  echo "  (summarize '$OLD_ON_OPEN', security '$OLD_SECURITY', audit '$OLD_AUDIT', diagrams"
  echo "  '$OLD_DIAGRAMS', 3D keyword view '$OLD_KEYWORDS', story '$OLD_STORY') are put back when it ends."
  yes_or_no "Change those six settings now (they are put back when the check ends)?" || {
    echo "Nothing was changed."
    exit 1
  }
  CHANGED=1
  clusia config set harness.on_open wait >/dev/null 2>&1
  clusia config set harness.check_security false >/dev/null 2>&1
  clusia config set harness.audit false >/dev/null 2>&1
  clusia config set harness.diagrams false >/dev/null 2>&1
  clusia config set harness.keywords_3d false >/dev/null 2>&1
  clusia config set harness.story false >/dev/null 2>&1
  check "the story box is off" test "$(clusia config get harness.story 2>/dev/null)" = "false"
  check "$PR is a pull request clusia can open" clusia open "$PR"
  start_window
}

step_static() {
  say "2. The static story and the call graph"
  echo "  Nothing here asks your agent: both are built from the repository with tree-sitter. The check"
  echo "  expects the first changed function to have a caller (the demo pull request does)."
  check "clusia story lists steps and ends with Static order" sh -c 'clusia story "$0" | tail -n 1 | grep -Eq "^Static order"' "$PR"
  if clusia story "$PR" 2>/dev/null | grep -Eq "Other files"; then
    pass "clusia story names an Other files step"
  else
    ask "No Other files step: did this PR change only source files?"
  fi
  check "clusia calls prints the centre and its callers at depth 3" sh -c 'clusia calls "$0" --depth 3 | grep -Eq "^●" && clusia calls "$0" --depth 3 | grep -Eq "^↑1"' "$PR"
  check "clusia calls --depth 1 prints no second level" sh -c '! clusia calls "$0" --depth 1 | grep -Eq "^(↑2|↓2)"' "$PR"
}

step_views() {
  say "3. Story and Calls in the window"
  echo "  Look at the window. Open the Diff tab and pick Story in the switch on the tab row."
  ask "Does Story list the changed functions as steps, each after the ones it calls, with Other files last, under a banner that says the order is static?"
  ask "Does a click on a step make it current, and does Mark read · next go on and tick it in the list?"
  echo "  Now pick Calls."
  ask "Does the left list show Changed in this PR with a tag per function, and does the canvas draw the first one with callers above (Up 1–3) and callees below (Down 1–3)?"
  ask "Is Depth 3, and does picking 2 or 4 redraw it? Does the status bar say Call graph from the worktree at <commit> · N changed, M touched by calls · depth d?"
  ask "Does a click on a node open its code in place with Full view and ×, does Enter or a double-click re-centre the graph on it, and does +N more expand a level?"
  ask "Does it work for a function in each of Rust, TypeScript and Python? A Go or other file should say Call graph not available for it yet, with Show in Unified and Open in Story."
}

step_write() {
  local status=0
  say "4. Write the story"
  echo "  This asks your agent for one turn that writes the words, the use cases and the order. If"
  echo "  it asks to run a command, answer in the window: the script never does."
  write_detached
  if wait_for "the story was written" 1800 story_ended; then
    wait_story
    status=$?
  else
    stop_story
    status=130
  fi
  cat "$ERR.out" 2>/dev/null
  if [ "$status" -eq 0 ]; then pass "clusia story --write finished (exit 0)"; else fail "clusia story --write ended with exit $status (130: stopped): $(cat "$ERR" 2>/dev/null)"; fi
  check "the Story says who wrote it and from which commit" sh -c 'clusia story "$0" | tail -n 1 | grep -Eq "^Written by .+ from commit [0-9a-f]{7}"' "$PR"
  ask "Does the Story now show In one sentence, the Before and After strip and the use cases as chips, and does every changed hunk belong to a step (the last may be Small and mechanical or Not placed by the agent)?"
}

step_comment() {
  say "5. Comment on a step and on a node"
  BEFORE_ITEMS="$(draft_items)"
  echo "  In Story, click Comment on this step and write a comment."
  wait_for "a draft item from the step comment appeared" 300 items_grew
  ask "Does the draft show the comment starting with On \"<the step's title>\":?"
  BEFORE_ITEMS="$(draft_items)"
  echo "  In Calls, click a node, then Comment in the side panel (try a node the PR did not touch too), and write a comment."
  wait_for "a draft item from the node comment appeared" 300 items_grew
  ask "For a changed function, does the comment land on its first changed lines, and for an untouched or outside one does it arrive as a general note naming path:lines?"
}

step_push() {
  say "6. A new commit"
  HEAD_BEFORE="$(story_head)"
  echo "  Push one small commit to the pull request's branch yourself (any file). The script does not."
  wait_for "the pull request head moved" 600 head_moved
  ask "Does Story say it was written for the older commit and offer Rewrite, and did nothing write on its own?"
}

step_rewrite() {
  say "7. Rewrite and compare"
  echo "  In Story, click Rewrite. It asks your agent for one more turn; answer any request in the window."
  wait_for "version 2 was written" 1800 version_is 2
  ask "Does the Story show Version 2 of 2 with the arrows, and are the steps that did not change marked as the same?"
  echo "  Tick Show changes since version 1."
  ask "Are steps that are new in version 2 dashed, changed ones outlined in amber with Since version 1: …, and removed ones listed struck through, with the legend and the count?"
}

step_setting() {
  say "8. The setting"
  echo "  Open Config › Harness."
  ask "Is there a Story row with the box Write the story when I open Story (off for this run) and the note One more turn when the Story opens?"
}

step_cli() {
  say "9. clusia story and clusia calls"
  check "the story is listed with its writer and commit" sh -c 'clusia story "$0" | grep -Eq "^Written by .+ from commit [0-9a-f]{7}"' "$PR"
  check "story --json has the versions, the steps and the head" sh -c 'clusia --json story "$0" | grep -Eq "\"steps\"" && clusia --json story "$0" | grep -Eq "\"head\""' "$PR"
  check "calls --json prints the graph" sh -c 'clusia --json calls "$0" --depth 3 | grep -Eq "\"center\""' "$PR"
  check "a bad option is a usage error (exit 2)" sh -c 'clusia story "$0" --nope >/dev/null 2>&1; test $? -eq 2' "$PR"
}

step_end() {
  say "10. The end"
  echo "  This discards the review of $PR on this Mac (its draft items are lost) and deletes the"
  echo "  story and the call graph. This check does not publish."
  if ! yes_or_no "Discard it now?"; then
    echo "  Skipped: the review was kept, with its draft, story and call graph. Discard it later with: clusia review discard $PR"
    return
  fi
  if clusia review discard "$PR" >/dev/null 2>"$ERR"; then
    pass "the review was discarded"
  else
    fail "the review was not discarded: $(cat "$ERR")"
  fi
  echo "  Opening it again starts clean: Story shows the static order (the settings are put back after this)."
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
step_static
step_views
step_write
step_comment
step_push
step_rewrite
step_setting
step_cli
step_end
summary
