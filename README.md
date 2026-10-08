<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/brand/clusia-mark-dark.svg">
    <img src="docs/assets/brand/clusia-mark.svg" alt="Clúsia" width="112">
  </picture>
</p>

<h1 align="center">Clúsia</h1>
<p align="center"><b>PR Reviews for Humans</b></p>

Clúsia is a macOS app for reviewing pull requests carefully, with your own AI tools at your side. It is free and open source.

- **A review you control.** Open a pull request in a local worktree. Write comments on the diff and keep them as a draft that follows the code when the author pushes again. Publish everything as one GitHub review when you are ready.
- **Your harness, your models.** Claude Code, Codex or a command of your own helps with diagrams, security notes, audits and tests (coming in the next sub-projects). Nothing is sent anywhere you did not choose.
- **Always at hand.** A menu bar popover shows what is waiting for you, which saved reviews went stale and how your review activity looks over the last weeks.

> **Status:** the daemon, the GitHub sync, the review engine, the CLI, the menu bar tray, the main window, notifications and the one-command install work today. The agent and the harness come next. Progress is tracked on the [project board](https://github.com/users/rzorzal/projects/1).

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/rzorzal/clusia/main/install.sh | bash
```

The script:

- builds Clúsia with [Homebrew](https://brew.sh) when you have it, and otherwise directly with Rust (installing the Xcode command line tools and Rust first if they are missing);
- runs `clusia install`, which puts **Clusia.app** in `/Applications` (or `~/Applications`), registers the background service to start at login, and links `clusia` into `/usr/local/bin` (or `~/.local/bin`);
- opens the app.

**Requirements:** macOS on Apple silicon or Intel, about 5 minutes and about 2 GB of free disk for the first build. Homebrew is optional. The script never uses `sudo`.

**Updating:** run the same command again. It replaces the app and keeps your settings and the notification permission.

| Option | Environment | What it does |
|---|---|---|
| `--dry-run` | `CLUSIA_DRY_RUN` | Prints every step and changes nothing |
| `--no-brew` | `CLUSIA_NO_BREW` | Builds directly even when Homebrew is installed |
| `--no-open` | `CLUSIA_NO_OPEN` | Does not open the app at the end |
| `--ref REF` | `CLUSIA_REF` | Installs a branch or tag instead of `main` |

Through the pipe, pass options after `bash -s --`:

```sh
curl -fsSL https://raw.githubusercontent.com/rzorzal/clusia/main/install.sh | bash -s -- --no-brew
```

**Homebrew by hand**, the same two steps the script runs:

```sh
brew install --HEAD rzorzal/clusia/clusia
clusia install --from "$(brew --prefix clusia)/libexec/bin"
```

**Read the script first:**

```sh
curl -fsSL https://raw.githubusercontent.com/rzorzal/clusia/main/install.sh -o install.sh
less install.sh
bash install.sh
```

**From a clone** of this repository (you need Git and the Rust toolchain pinned in `rust-toolchain.toml`):

```sh
cargo run --release -p clusia -- install
```

`clusia install --dry-run` shows what it would do.

## First steps

1. Open **Clusia.app**. The menu bar tray, the background service and the window come up. A few seconds later macOS asks to allow notifications; choose **Allow**.
2. Sign in to GitHub: the window uses your `gh` login, or run `clusia auth login` and give it a personal access token on stdin.
3. On a new machine the window opens on a short first run: connect GitHub, point Clúsia at the folders with your clones, and see which AI harness is installed. Press **Continue**.
4. Open a pull request from the tray, from Home, or with `clusia open owner/repo#123`.
5. Click a line to comment (Shift-click for a range) and reply to or resolve threads. Comments are rich text, with a toolbar, emoji, GIFs (add a Giphy key in Config › Media) and a preview of what GitHub will show.
6. Press **Finalize review**. Everything goes to GitHub as one review; nothing is posted before that. Closing a tab or the window with an unpublished draft asks first.

<p align="center"><img src="docs/assets/sp1-m5b-review-light.png" alt="The Clúsia review screen: PR header, sections, a syntax-highlighted diff with a thread and a draft comment, and the draft panel" width="720"></p>

<p align="center"><img src="docs/assets/sp1-m4-popover.png" alt="The Clúsia menu bar popover in light and dark mode: activity heatmap, counters, search, repository chips and paginated pull request lists" width="640"></p>

## Everyday commands

Commands that talk to the daemon start it when it is not running; `daemon status`, `daemon stop`, `install` and `uninstall` do not. `clusia --help` lists the rest (`sync`, `worktree`, `activity`, `review status|note|edit|rm|close|discard`). Add `--json` for machine-readable output.

| Command | What it does |
|---|---|
| `clusia auth status` / `login` / `logout` | Show which token is used, store one in the Keychain (read from stdin), or remove it |
| `clusia prs [--assigned] [--mine]` | List pull requests where your review is requested and the ones you opened |
| `clusia open owner/repo#123` | Refresh, prepare the worktree and show what is new (a pull request URL works too) |
| `clusia review comment owner/repo#123 src/lib.rs:42 "text"` | Add a line comment (`path:line` or `path:start-end`) to the draft |
| `clusia review publish owner/repo#123 --verdict approve` | Publish the draft as one review (`approve`, `request-changes`, `comment` or `close`) |
| `clusia config get <key>` / `set <key> <value>` | Read or change one setting, for example `github.poll_interval_secs` |
| `clusia daemon status` / `stop` | Show whether the daemon runs, or stop it |
| `clusia install` / `uninstall` | Put the app, the login agent and the `clusia` link in place, or remove them |

## Notifications and login

Clúsia notifies you when:

- you are asked to review a pull request;
- new commits arrive on a pull request you reviewed;
- someone replies in a thread you started or joined;
- someone mentions you;
- CI fails on a pull request you opened;
- syncing with GitHub has a problem (an expired token, no connection for more than 10 minutes, a rate limit);
- a damaged settings or state file was set aside.

In Config › Notifications each event has its own switches for the tray list, macOS banners and sound. There are four soft bundled sounds (with a preview), **Do not disturb** hours and weekdays, *Follow macOS Focus*, *Group bursts* (updates to one pull request within 2 minutes become one banner), and a button that sends a test notification. Do not disturb silences banners and sound, never the tray list.

Config › General has **Start at login**. **Quit** in the tray stops everything (tray, window and daemon) until you open Clusia.app again.

<p align="center"><img src="docs/assets/sp1-m6-config-notifications-light.png" alt="Config › Notifications: for each event, whether it reaches the tray, macOS and plays a sound, the sound, Do not disturb hours and weekdays, Follow macOS Focus, Group bursts and the macOS permission" width="720"></p>

## Where things live

| What | Where |
|---|---|
| The app | `/Applications/Clusia.app` (or `~/Applications`) |
| The `clusia` command | a link in `/usr/local/bin` (or `~/.local/bin`) |
| Reviews and settings | `~/Library/Application Support/Clusia` |
| Logs | `~/Library/Logs/Clusia/daemon.log`, `app.log`, `tray.log`, and `daemon.launchd.log` for what the login agent printed |
| The login agent | `~/Library/LaunchAgents/io.github.rzorzal.clusia.daemon.plist` |

[`docs/daemon.md`](docs/daemon.md) has the environment variables, files and log rotation.

**Privacy:** your GitHub token comes from `gh` or the macOS Keychain and never appears in logs or output. Clúsia talks to GitHub and, if you add a key, to Giphy, and to nothing else. Your own clone is never checked out or branched: Clúsia only adds refs under `refs/clusia/` and works in separate worktrees.

## Troubleshooting

**No notifications.** Check System Settings › Notifications › Clúsia. Config › Notifications shows the permission Clúsia sees and can send a test.

**`clusia: command not found`.** The link went to `~/.local/bin` and that folder is not on your `PATH`. Add `export PATH="$HOME/.local/bin:$PATH"` to your shell profile.

**The tray is gone after a crash.** Open Clusia.app again. launchd restarts the daemon by itself after a crash.

**Something looks wrong.** Read the logs in `~/Library/Logs/Clusia`, starting with `daemon.log`.

**Stop everything.** Choose Quit in the tray, or run `clusia daemon stop`.

**Start fresh.** Run `clusia uninstall`, move `~/Library/Application Support/Clusia` to the Trash, and install again. This deletes your saved drafts and settings.

## Uninstall

```sh
clusia uninstall
brew uninstall clusia    # only if you installed through Homebrew
```

`clusia uninstall` removes the app, the login agent and the `clusia` link. Your reviews and settings stay in `~/Library/Application Support/Clusia` until you delete that folder.

Builds from before the ad hoc signing may have left a signing identity in your login keychain. Remove it with:

```sh
security delete-identity -c "Clúsia Local"
```

## The name and the mark

*Clusia* is a genus of tropical American plants, named after the 16th-century botanist Carolus Clusius, who was known for describing plants with great care. In Brazil, *Clusia fluminensis* grows on the restinga, the sandy coast where salt, wind and strong sun leave little else standing. It manages with thick, waxy leaves and with a kind of photosynthesis that takes in air at night to save water.

One of its relatives, *Clusia rosea*, is known as the **autograph tree**: whatever you write on its leaves stays there, without harming the plant. That is what a good review does. It leaves a clear, lasting note on someone else's work and makes it stronger.

The mark is that leaf. The two written lines are review comments, and the orange point is the one thing that still needs attention. The colors carry the same meaning in the app: green for additions and approval, orange for removals and risk.

<p align="center"><img src="docs/assets/brand/overview.png" alt="The Clúsia mark on light and dark backgrounds, the app icon, and the menu bar icon with and without the new-activity dot" width="720"></p>

Brand files (SVG, menu bar templates at 1× and 2×, app icon `.icns`) live in [`docs/assets/brand`](docs/assets/brand). The accent colors are green `#2F9E44` / `#4CC35F` and orange `#D9572B` / `#F07A4A` (light / dark surfaces).

## How it is built

Clúsia is a Rust workspace with four programs that talk over a local Unix socket:

| Program | What it does |
|---|---|
| `clusiad` | The background service. It syncs with GitHub, manages worktrees, keeps review drafts and publishes reviews. It is the only part that holds state or secrets. |
| `clusia-tray` | The menu bar icon and popover (native AppKit). It is the app you open (Clusia.app), the only part that posts notifications, and it keeps the daemon running. |
| `clusia-app` | The main window (Bevy): Home, Config and the review screen (diff in Unified or Split with syntax colors, rich-text comments with one composer: formatting, emoji, Giphy GIFs, image links and a preview; finalize), plus a first run for a new machine. Opening it starts the daemon if needed, and a second launch brings the open window forward. |
| `clusia` | The command-line interface. |

## Development

Every change passes the same gate:

```sh
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```

Tests never call the real GitHub, the real `gh` or the real Keychain. Other scripts in `scripts/`:

- `m6-check.sh`: the owner check for install, launch and notifications, run on a real Mac with someone at the keyboard (it stops the daemon and the tray);
- `flake-hunt.sh [RUNS]`: runs the suites that talk to a daemon over and over and names the tests that failed;
- `leak-check.sh`: runs the whole suite and fails if it leaves a `clusiad` running on a temporary home.

The window runs on demo data, with no daemon and nothing saved, with `target/debug/clusia-app --demo --dark`. Plans and specs live in the GitHub issues: the design spec is [#2](https://github.com/rzorzal/clusia/issues/2).

## Credits

Emoji graphics from [Twemoji](https://github.com/jdecked/twemoji), © Twitter, Inc. and other contributors, licensed under [CC-BY 4.0](https://creativecommons.org/licenses/by/4.0/). Resized for use in Clúsia. Emoji names and shortcodes from [emojibase](https://github.com/milesj/emojibase) (MIT). Fonts: Inter and JetBrains Mono (SIL OFL 1.1). GIF search is powered by [GIPHY](https://giphy.com/). The licences and links are also under Config › About.

## License

[Apache-2.0](LICENSE)
