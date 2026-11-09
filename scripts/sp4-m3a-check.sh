#!/usr/bin/env bash
# Owner check for the system's engine and command line, on a pull request of yours
# (rzorzal/*). It refuses every other owner. It starts the pull request's code in a container of
# the engine you already have, publishes its ports on 127.0.0.1 only, and sends requests to
# them. It approves nothing for you (each approval is a question you answer at the terminal),
# installs nothing, starts no engine, changes no setting, removes no Docker object itself and
# publishes nothing to GitHub. It discards the review only when you say yes.
#
#   scripts/sp4-m3a-check.sh [rzorzal/blog#37]    run the check (asks first)
#   scripts/sp4-m3a-check.sh --list               print the steps and exit
#
# Needs Clúsia installed (clusia on the PATH, signed in to GitHub), an agent set up in
# Config › Harness, Docker or Colima running, and a pull request whose change a reviewer can
# start and call. If the pull request already has a review of yours on this Mac, its draft is
# lost at the end when you let the script discard it.
set -u

PR=""
OWNER=""
REPO=""
NUMBER=""
KEY=""
LIST=0
passed=0
failed=0
# Clúsia's data folder, as the CLI finds it.
DATA="${CLUSIA_HOME:-$HOME/Library/Application Support/Clusia}"
ERR="$(mktemp -t sp4-m3a-check.XXXXXX)" || {
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
  "open: the review opens; clusia system status says \"Not started\" or names what is missing"
  "generate: clusia tests --generate; the recipe has [system]; clusia system endpoints lists the endpoints, or says why there are none"
  "plan: clusia system start prints the plan's SYSTEM section (start, health, ports, variables, network, endpoints); you read it and answer"
  "start: the stages print, then Running (or Unhealthy) with each port line and URL"
  "loopback: docker port of the forwarder shows only 127.0.0.1, and lsof shows nothing listening on * for those ports"
  "curl: the curl that status prints runs in your shell and answers"
  "send: clusia system send 1 prints the status, time, headers and a bounded body"
  "logs: clusia system logs --follow; Ctrl-C exits 130 and status still says Running"
  "network: clusia system network on restarts it; off restarts it again; docker inspect shows the publish network only while on"
  "edit-guard: change the recipe's [system] by hand; clusia system restart refuses until the plan is approved again"
  "stop: clusia system stop; docker ps -a --filter label=io.clusia.kind=system lists nothing"
  "end: start again, then discard the review; no clusia-system container or network is left"
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

# Whether the system's JSON view holds `$1` (spaces and newlines removed).
status_has() {
  clusia --json system status "$PR" 2>/dev/null | tr -d ' \n' | grep -q "$1"
}

status_is_up() {
  status_has '"state":"\(running\|unhealthy\)"'
}

# No container or network of this review's system is left, when this Mac has Docker to ask.
no_system_object() {
  command -v docker >/dev/null 2>&1 || return 0
  [ -z "$(docker ps -aq --filter label=io.clusia.kind=system --filter "label=io.clusia.review=$KEY" 2>/dev/null)" ] \
    && [ -z "$(docker network ls -q --filter label=io.clusia.kind=system --filter "label=io.clusia.review=$KEY" 2>/dev/null)" ]
}

system_names() {
  docker ps --format '{{.Names}}' --filter label=io.clusia.kind=system --filter "label=io.clusia.review=$KEY" 2>/dev/null
}

# The forwarder's name, and the system's own.
fwd_name() { system_names | grep -- '-fwd$' | head -1; }
system_name() { system_names | grep -v -- '-fwd$' | head -1; }

# Every published port of the forwarder is on 127.0.0.1.
only_loopback() {
  local fwd mapped
  fwd="$(fwd_name)"
  [ -n "$fwd" ] || return 1
  mapped="$(docker port "$fwd" 2>/dev/null)"
  [ -n "$mapped" ] || return 1
  ! printf '%s\n' "$mapped" | grep -qv '127.0.0.1:'
}

# Nothing listens on a wildcard address for the forwarder's host ports (true without lsof).
no_wildcard_listener() {
  local fwd port listening
  command -v lsof >/dev/null 2>&1 || { echo "lsof is not installed; skipped" >&2; return 0; }
  fwd="$(fwd_name)"
  [ -n "$fwd" ] || return 1
  for port in $(docker port "$fwd" 2>/dev/null | sed -n 's/.*:\([0-9][0-9]*\)$/\1/p'); do
    listening="$(lsof -nP -iTCP:"$port" -sTCP:LISTEN 2>/dev/null)"
    if printf '%s\n' "$listening" | grep -q -e "\*:$port" -e "0.0.0.0:$port" -e '\[::\]'; then
      return 1
    fi
  done
  return 0
}

# The system's container is attached to the publish network.
pub_attached() {
  docker inspect "$(system_name)" --format '{{range $k, $v := .NetworkSettings.Networks}}{{$k}} {{end}}' 2>/dev/null | grep -q -- '-pub'
}

not_pub_attached() {
  ! pub_attached
}

# A restart of an edited recipe is refused.
restart_refused() {
  ! clusia system restart "$PR" 2>/dev/null
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

echo "This opens a real review of $PR and drives clusia system: one agent turn of YOUR agent to"
echo "generate the recipe, then the pull request's code runs in a container of the engine you"
echo "already have, with its ports on 127.0.0.1 only, and the script sends requests to it. Every"
echo "approval is a question you answer at the terminal; the script approves nothing. It installs"
echo "nothing, starts no engine and changes no setting. It asks before it discards the review of"
echo "$PR on this Mac at the end. It publishes nothing to GitHub."
printf 'Continue? [y/N] '
read -r answer || answer=n
case "$answer" in y | Y | yes) ;; *) exit 1 ;; esac

say "${STEPS[0]}"
command -v clusia >/dev/null 2>&1 || { fail "clusia is on the PATH"; exit 1; }
check "the review opens" clusia open "$PR"
clusia system status "$PR"
ask "It says \"Not started\" or names what is missing"

say "${STEPS[1]}"
go_on "run clusia tests $PR --generate (one agent turn on your account)"
clusia tests "$PR" --generate
clusia system endpoints "$PR"
check "the recipe has a system section" status_has '"spec":{'
ask "The endpoints are listed, or the output says why there are none"

say "${STEPS[2]}"
echo "  The next command prints the SYSTEM section of the plan. Read it, then answer."
say "${STEPS[3]}"
go_on "run clusia system start $PR (it sets up what you approve and starts the system in a container)"
clusia system start "$PR"
code=$?
echo "  clusia exited $code (0 running, 1 failed, 124 unhealthy, 130 stopped)."
check "it is Running or Unhealthy" status_is_up
ask "Each stage printed, then a port line and a URL for every port"

say "${STEPS[4]}"
check "the forwarder publishes on 127.0.0.1 only" only_loopback
check "nothing listens on a wildcard address for those ports" no_wildcard_listener

say "${STEPS[5]}"
clusia system status "$PR"
ask "The curl under the first endpoint answers when you run it in your shell"

say "${STEPS[6]}"
go_on "run clusia system send $PR 1 (one request to the system's port on 127.0.0.1)"
clusia system send "$PR" 1
ask "It printed a status, a time, headers and a body that stops at 64 KB"

say "${STEPS[7]}"
go_on "run clusia system logs $PR --follow (press Ctrl-C to end it; the system keeps running)"
trap 'true' INT
clusia system logs "$PR" --follow
code=$?
trap 'exit 130' INT TERM HUP
check "logs --follow ended with 130" test "$code" = 130
check "the system still runs" status_has '"state":"running"'

say "${STEPS[8]}"
go_on "run clusia system network $PR on, then off (each restarts the system)"
clusia system network "$PR" on
check "the publish network is attached while on" pub_attached
clusia system network "$PR" off
check "the publish network is detached while off" not_pub_attached

say "${STEPS[9]}"
go_on "append a comment line to the recipe of $PR and run clusia system restart (it must refuse)"
printf '\n# edited by the check\n' >>"$DATA/worktrees/$KEY/.clusia/recipe.toml"
check "restart refuses until the plan is approved again" restart_refused
echo "  Read the plan again and answer."
go_on "run clusia system start $PR (it prints the plan again and asks)"
clusia system start "$PR"

say "${STEPS[10]}"
go_on "run clusia system stop $PR"
clusia system stop "$PR"
check "no system container or network is left" no_system_object

say "${STEPS[11]}"
go_on "run clusia system start $PR again"
clusia system start "$PR"
if yes_or_no "Discard the review of $PR on this Mac now"; then
  check "the review is discarded" clusia review discard "$PR"
  sleep 2
  check "no system container or network is left" no_system_object
else
  echo "  Skipped: the review was kept."
fi

printf '\n%d passed, %d failed\n' "$passed" "$failed"
[ "$failed" = 0 ]
