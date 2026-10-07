# The daemon: environment, files and logs

`clusiad` is started on demand by `clusia`, `clusia-tray` and `clusia-app` (and at login by
the LaunchAgent). It is one process per data folder; a lock file and the socket keep a second
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
does not hide a Homebrew install.

| Variable | Meaning |
|---|---|
| `CLUSIA_HOME` | the data folder (default `~/Library/Application Support/Clusia`) |
| `CLUSIA_GITHUB_API` | GitHub API base URL (GitHub Enterprise, tests) |
| `CLUSIA_GITHUB_TOKEN` | a token that wins over `gh` and the Keychain |
| `CLUSIA_GH_BIN` | the `gh` program, overriding the lookup |
| `CLUSIA_SECRET_STORE=memory` | keep secrets in memory instead of the Keychain (tests) |
| `CLUSIA_TRAY_BIN` | the tray program; `none` disables it |
| `CLUSIA_DAEMON_BIN` | the daemon program the clients start |
| `CLUSIA_GIPHY_API` | Giphy API base URL |

## Files

| File | What |
|---|---|
| `<data>/clusiad.sock` | the socket, created private (mode 0600) |
| `<data>/clusiad.lock` | held (`flock`) by the running daemon; the OS drops it when the daemon dies |
| `~/Library/Logs/Clusia/daemon.log` | the daemon's log |
| `~/Library/Logs/Clusia/tray.log` | the tray's output |
| `~/Library/Logs/Clusia/daemon.start.log` | what the last start printed before the log opened |

Each log starts a new file every day: yesterday's becomes `daemon.YYYY-MM-DD.log`, and the
current file plus the six before it are kept. Logs are readable by the owner only.
