#!/bin/bash
# Installs or updates Clusia on this Mac and opens it:
#   curl -fsSL https://raw.githubusercontent.com/rzorzal/clusia/main/install.sh | bash
# Options: --no-brew  --dry-run  --no-open  --ref REF   (env: CLUSIA_NO_BREW, CLUSIA_DRY_RUN,
# CLUSIA_NO_OPEN, CLUSIA_REF). It never asks for a password itself and leaves your Clusia data alone.
#
# Everything lives inside functions and `main` runs on the last line, so a download that is
# cut short runs nothing.
set -euo pipefail

REPO="https://github.com/rzorzal/clusia.git"

say() { printf '==> %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit "${2:-1}"; }

# Runs a command, or only prints it under --dry-run.
run() {
  if [ -n "$DRY_RUN" ]; then
    printf '+'
    printf ' %q' "$@"
    printf '\n'
  else
    "$@"
  fi
}

# Homebrew builds Clusia and its dependencies; `clusia install` then does the rest.
install_with_brew() {
  if brew list --formula clusia >/dev/null 2>&1; then
    say "Updating Clúsia with Homebrew"
    run brew upgrade --fetch-HEAD clusia
  else
    say "Building Clúsia with Homebrew (about 5 minutes)..."
    run brew install --HEAD rzorzal/clusia/clusia
  fi
  BREW=1
  # Bare `brew --prefix` works before the formula exists, which a dry run needs;
  # opt/clusia is the stable link Homebrew keeps to the installed version.
  FROM="$(brew --prefix)/opt/clusia/libexec/bin"
}

install_build_tools() {
  if ! xcode-select -p >/dev/null 2>&1; then
    say "Installing the Xcode command line tools"
    run xcode-select --install || true
    [ -n "$DRY_RUN" ] || die "finish the Xcode command line tools install in the window that opened, then run me again"
  fi
  if ! command -v cargo >/dev/null 2>&1; then
    say "Installing Rust with rustup"
    if [ -n "$DRY_RUN" ]; then
      echo "+ curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal"
    else
      curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
      # rustup puts cargo in ~/.cargo/bin, which this shell does not have on PATH yet.
      # shellcheck disable=SC1091
      . "$HOME/.cargo/env"
    fi
  fi
}

build_directly() {
  install_build_tools
  if command -v cargo >/dev/null 2>&1 && ! command -v rustup >/dev/null 2>&1; then
    say "Building with the cargo on PATH; Clúsia needs Rust 1.95 or newer"
  fi
  local src="<temp dir>"
  if [ -z "$DRY_RUN" ]; then
    TMP="$(mktemp -d "${TMPDIR:-/tmp}/clusia.XXXXXX")"
    src="$TMP"
  fi
  say "Building Clúsia (about 5 minutes)..."
  run git clone --depth 1 --branch "$REF" "$REPO" "$src"
  if [ -n "$DRY_RUN" ]; then
    echo "+ (cd $src && cargo build --release)"
  else
    (cd "$src" && cargo build --release)
  fi
  FROM="$src/target/release"
}

main() {
  DRY_RUN="${CLUSIA_DRY_RUN:-}"
  local no_brew="${CLUSIA_NO_BREW:-}" no_open="${CLUSIA_NO_OPEN:-}"
  REF="${CLUSIA_REF:-main}"
  TMP=""
  FROM=""
  BREW=""
  while [ $# -gt 0 ]; do
    case "$1" in
      --no-brew) no_brew=1 ;;
      --dry-run) DRY_RUN=1 ;;
      --no-open) no_open=1 ;;
      --ref) [ $# -ge 2 ] || die "--ref needs a branch or tag" 2; REF="$2"; shift ;;
      *) die "unknown option: $1 (use --no-brew, --dry-run, --no-open or --ref REF)" 2 ;;
    esac
    shift
  done

  [ "$(uname -s)" = "Darwin" ] || die "Clúsia is macOS only"
  trap '[ -z "$TMP" ] || rm -rf "$TMP"' EXIT

  if [ -z "$no_brew" ] && command -v brew >/dev/null 2>&1; then
    install_with_brew
  else
    build_directly
  fi

  say "Installing Clúsia.app, the login agent and the clusia command"
  # The folder is chosen here, not by `clusia install`, so `open` gets the very bundle
  # that was just written instead of whichever app LaunchServices calls "Clusia".
  local apps=/Applications
  [ -w "$apps" ] || apps="$HOME/Applications"
  run "$FROM/clusia" install --from "$FROM" --applications "$apps"
  [ -n "$no_open" ] || run open "$apps/Clusia.app"

  if [ -n "$DRY_RUN" ]; then
    say "Dry run: nothing was changed."
    return
  fi
  say "Done. Clusia.app is in $apps."
  if [ -n "$BREW" ]; then
    say "To remove it: clusia uninstall, then brew uninstall clusia"
  else
    say "To remove it: clusia uninstall"
  fi
  say "Run this command again any time to update."
}

main "$@"
