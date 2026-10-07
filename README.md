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

> **Status:** early development, not ready for daily use yet. The daemon, the GitHub sync, the review engine, the CLI, the menu bar tray and the main window work today: open a pull request, read its diff, comment on lines, reply to and resolve threads, and publish one GitHub review. The agent and the harness come next. Progress is tracked on the [project board](https://github.com/users/rzorzal/projects/1).

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
| `clusia-tray` | The menu bar icon and popover (native AppKit). The daemon starts it, and it exits when the daemon stops. |
| `clusia-app` | The main window (Bevy): Home, Config and the review screen (diff in Unified or Split with syntax colors, rich-text comments with one composer: formatting, emoji, Giphy GIFs, image links and a preview; finalize), plus a first run for a new machine. Opening it starts the daemon if needed, and a second launch brings the open window forward. |
| `clusia` | The command-line interface. |

<p align="center"><img src="docs/assets/sp1-m5b-review-light.png" alt="The Clúsia review screen: PR header, sections, a syntax-highlighted diff with a thread and a draft comment, and the draft panel" width="720"></p>

<p align="center"><img src="docs/assets/sp1-m4-popover.png" alt="The Clúsia menu bar popover in light and dark mode: activity heatmap, counters, search, repository chips and paginated pull request lists" width="640"></p>

Your own clone is never checked out or branched: Clúsia only adds refs under `refs/clusia/` and works in separate worktrees. Your GitHub token comes from `gh` or the macOS Keychain and never appears in logs or output.

## Try it from source

You need macOS, Git, and the Rust toolchain (the version is pinned in `rust-toolchain.toml`).

```sh
cargo build --workspace
target/debug/clusia auth status        # uses your gh login, or: clusia auth login (token on stdin)
target/debug/clusia prs                # pull requests assigned to you and opened by you
target/debug/clusia open owner/repo#123
target/debug/clusia review comment owner/repo#123 src/lib.rs:42 "Is this branch reachable?"
target/debug/clusia review publish owner/repo#123 --verdict request-changes
```

Every `clusia` command starts the daemon when it is not running.

The window:

```sh
target/debug/clusia-app                                    # Home, live from the daemon
target/debug/clusia-app --review owner/repo#123            # open a review
target/debug/clusia-app --config                           # Config
target/debug/clusia-app --demo --dark                      # demo data, no daemon, nothing saved
target/debug/clusia-app --demo --review rzorzal/clusia#123 # the demo review
```

In a review, click a line to comment (Shift-click for a range), reply to or resolve threads under Comments, then **Finalize review**: everything goes to GitHub as one review, and nothing is posted before you publish. Closing a tab or the window with an unpublished draft asks first; the tray reminds you about reviews kept for later.

Comments are rich text everywhere. The composer (under a line, in a reply, in the finalize form) has a toolbar for bold, italic, code, link, list, quote and suggestion, an **Emoji** picker (Twemoji, so they look the same on every Mac), a **GIF** picker backed by Giphy (add your own key in Config › Media; without one you can still paste a link) and **Image** links, and **Preview** renders exactly what GitHub will show. Pictures from other sites appear as links unless you turn on *Load images from other sites* in Config › Media.

<p align="center"><img src="docs/assets/sp1-m5c-composer-light.png" alt="The Clúsia composer under line 44 with a typed comment, the toolbar and the emoji picker open" width="720"></p>
<p align="center"><img src="docs/assets/sp1-m5c-rendered-dark.png" alt="A comment in the dark theme with bold text, inline code, a code block, a link, an emoji and a playing GIF" width="720"></p>

On a new machine, or whenever the GitHub login is missing, the window opens on a short first run: connect GitHub (the `gh` login or a token), point Clúsia at the folders with your clones, and see which AI harness is installed. It goes away when the login works and you press **Continue**.

<p align="center"><img src="docs/assets/sp1-m5c-first-run-light.png" alt="The first-run screen: connect GitHub, where your repositories are, and the AI harness" width="720"></p>

## Development

Every change passes the same gate:

```sh
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```

Tests never call the real GitHub, the real `gh` or the real Keychain. Plans and specs live in the GitHub issues: the design spec is [#2](https://github.com/rzorzal/clusia/issues/2).

## Credits

Emoji graphics from [Twemoji](https://github.com/jdecked/twemoji), © Twitter, Inc. and other contributors, licensed under [CC-BY 4.0](https://creativecommons.org/licenses/by/4.0/). Resized for use in Clúsia. Emoji names and shortcodes from [emojibase](https://github.com/milesj/emojibase) (MIT). Fonts: Inter and JetBrains Mono (SIL OFL 1.1). GIF search is powered by [GIPHY](https://giphy.com/). The licences and links are also under Config › About.

## License

[Apache-2.0](LICENSE)
