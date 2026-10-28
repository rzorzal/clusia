# UI mockups

Approved mockups for the Clúsia window, tray popover and config screens (issue #12).

- `png/` — renders of every board (light, plus dark for Home, Review, Calls, Plugins and the tray).
- `src/` — the board sources (`*.dc.html`) and `canvas.json` from the design canvas. They are the
  reference for M5 (window) and later milestones: layout, copy, tokens and states.

Live canvas (private to the owner): https://claude.ai/artifact/CWvYNxAqgnKrrLqi81LqPQ

## Tokens

| Token | Light | Dark |
|---|---|---|
| bg | `#F6F7F5` | `#121514` |
| chrome | `#EEF1EE` | `#181C1A` |
| surface | `#FFFFFF` | `#1A1E1C` |
| line | `#E2E6E2` | `#2A302C` |
| fg | `#1B1F1C` | `#E9ECE9` |
| muted | `#5D665F` | `#9AA49C` |
| faint | `#8A938C` | `#6B746D` |
| green | `#2F9E44` | `#4CC35F` |
| orange | `#D9572B` | `#F07A4A` |

Type: Inter (UI), JetBrains Mono (code). 6 px radius, minimal borders.

## SP4 · M1 (Tests)

`sp4/` holds the M1 Tests boards approved in #311: the empty Tests tab, Auto Tests, Run Function tests, the setup plan and Config › Tests, in light and dark. The sources are plain HTML (`sp4/src`, sharing `base.css`); render them with headless Chrome at 1282×802. These boards replace `Tests.png` for M1; `TestCockpit.png` stays the target for M2.
