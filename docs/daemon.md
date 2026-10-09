# The daemon: environment, files and logs

`clusiad` is started on demand by `clusia`, `clusia-tray` and `clusia-app` (and at login by
the LaunchAgent). For the default data folder, when the LaunchAgent is loaded, they ask launchd
to start it (`launchctl kickstart`), so launchd restarts it after a crash; otherwise they start
it themselves. It is one process per data folder; a lock file and the socket keep a second
one from starting.

## Which environment the daemon sees

A daemon inherits the environment of whoever started it, and keeps it until it stops:

| Started by | Environment |
|---|---|
| a terminal (`clusia prs`, `clusia daemon start`) | that shell's, including `PATH` and any `CLUSIA_*` variable |
| the app or the tray opened from Finder or the Dock | launchd's: a bare `PATH` (`/usr/bin:/bin:/usr/sbin:/sbin`) |
| the LaunchAgent (login, restart after a crash) | the plist's `EnvironmentVariables`: `PATH=/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin` |

Because the first start wins, a `CLUSIA_*` variable exported in one terminal does nothing for a
daemon that is already running: stop it (`clusia daemon stop`) and start it again from where
the variable is set.

`gh` and the agent tools (`claude`, `codex`) are looked up in order: the folders of `PATH`,
`~/.local/bin`, `~/.claude/local`, `/opt/homebrew/bin`, `/usr/local/bin`. A bare `PATH` therefore
does not hide a Homebrew install. The agent's turns start the program of `harness.program` (default: `claude` on the `PATH`) with the daemon's environment minus `GH_TOKEN`, `GITHUB_TOKEN`, `GH_ENTERPRISE_TOKEN` and every `CLUSIA_*` variable.

The agent's turns run in `--permission-mode default` and ask `clusiad permission-bridge`, a small stdio server that `claude` itself starts (from `--mcp-config`, with `--strict-mcp-config`, so your own MCP servers are not loaded in those turns). The bridge forwards each request over the daemon's socket and only the daemon decides: no answer, an error or a bridge that cannot reach it means deny. With `harness.sandbox` on (default), the turn also gets `--settings` with the sandbox enabled (no network, writes only in the worktree). `harness.permission_timeout_secs` (default 120, 30 to 600) is how long a request waits.

| Variable | Meaning |
|---|---|
| `CLUSIA_HOME` | the data folder (default `~/Library/Application Support/Clusia`) |
| `CLUSIA_GITHUB_API` | GitHub API base URL (GitHub Enterprise, tests) |
| `CLUSIA_GITHUB_TOKEN` | a token that wins over `gh` and the Keychain |
| `CLUSIA_GH_BIN` | the `gh` program, overriding the lookup |
| `CLUSIA_SECRET_STORE=memory` | keep secrets in memory instead of the Keychain (tests) |
| `CLUSIA_TRAY_BIN` | the tray program; `none` disables it |
| `CLUSIA_CLAUDE_BIN` | the `claude` program the review agent runs when Config › Harness names none |
| `CLUSIA_DAEMON_BIN` | the daemon program the clients start |
| `CLUSIA_GIPHY_API` | Giphy API base URL |

## Files

| File | What |
|---|---|
| `<data>/clusiad.sock` | the socket, created private (mode 0700: the daemon's umask is 077) |
| `<data>/clusiad.lock` | held (`flock`) by the running daemon; the OS drops it when the daemon dies |
| `~/Library/Logs/Clusia/daemon.log` | the daemon's log |
| `~/Library/Logs/Clusia/tray.log` | the tray's output |
| `~/Library/Logs/Clusia/app.log` | the window's log |
| `~/Library/Logs/Clusia/daemon.start.log` | what each start printed before the log opened; appended to, emptied once past 256 KiB |
| `<data>/agent/<owner>~<repo>~<n>.jsonl` | the agent's chat log of a review (every event; 0600; at most 2 MB, then the oldest half is dropped) |
| `<data>/agent/<owner>~<repo>~<n>.state.json` | the suggestions you accepted or dismissed, the head the last summary saw and the review's permission rules (`Bash(cargo test:*)`, `Edit`), which end with the session |
| `<worktree>/.clusia/review.md` | what the agent reads first: the pull request, the draft, the summary (excluded from git through the worktree's `info/exclude`). Never written through a symlink: when the pull request makes `.clusia` anything but a folder, the notes are not written |
| `~/Library/Logs/Clusia/daemon.launchd.log` | what a daemon launchd started printed (the login agent's output); not rotated |

Each log starts a new file every day: yesterday's becomes `daemon.YYYY-MM-DD.log`, and the
current file plus the six before it are kept. Logs are readable by the owner only. With
`--home` or `CLUSIA_HOME` the logs go to `<data>/logs/` instead of `~/Library/Logs/Clusia/`.
