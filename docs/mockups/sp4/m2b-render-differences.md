# SP4 · M2b: how the renders differ from the mockups

The renders are `docs/assets/sp4-m2b-<scene>-{light,dark}.png`, made with
`clusia-app --demo --scene cockpit-<scene> [--light|--dark] --screenshot <png>`. The mockups are
`docs/mockups/sp4/png/{,Dark-}Cockpit-<scene>.png`. These differences are known and accepted:

| Where | Mockup | Render | Why |
|---|---|---|---|
| Window size | 1280 × 800 with the macOS title bar | 1280 × 726, the window's content | the screenshot is the app's own surface; ruled: Errors sits below the fold at the render height; the center scrolls and the action row is pinned |
| Window chrome | traffic lights and a narrower tab bar | the app's tab bar and Config button; the header, section tabs, sub-tab bar and status bar are hidden inside the cockpit | the render is the real window; the cockpit takes the tab's whole body |
| Demo review | `auth refresh`, main at `4e1f2a0` | `feat: auth refresh`, main at `4e1f2a0` | the demo's pull request title; every header reads `rzorzal/clusia #123` |
| Benchmark signature | `pub fn backoff_delay(attempt: u32, base: Duration) -> Duration` | `fn backoff_delay(attempt: int, base: text)` | the window draws `fn <qualified>(<param>: <kind>, …)` from the parameters and has no return type |
| Benchmark values | `base` beside `attempt` | `base` on its own row | the fields are one per row in the kit's grid |
| Script pane | no cap, no editor affordance | read-only, at most 12 lines with "+ k more lines" | the script is edited in `.clusia/e2e/` and the CLI, not in the window |
| Loop file | `.clusia/functions/backoff-delay/bench.rs` | the path of the demo's loop file | the path is the one the fixture gives; no text pane is drawn |
| Charts | smooth SVG polylines, a jagged CPU line, a flat line mid-plot | Bevy UI segments stepped by 1 px, main's dash in 5 px pieces; the plot starts at 0, so a flat series sits near the top, and the CPU of the demo is a flat line | the plot is drawn from rectangles with a zero-based axis (top = larger side × 1.1) |
| Chart height | a plot about 130 px tall with Errors under it | the plot is cut by the center's scroll area at 726 px | see the window size |
| Chart legend | "live" in green | a spinner and "live" in green | the running state is shown by the kit's spinner |
| Table values | `190 MB ±6` | `190.0 MB ±6`; spreads without the unit of the value | the table formats every value with one decimal and drops the unit from ± |
| Comparison note | "Both sides at this head and base · container · no network" in two lines | the same text in one or two lines by width | the right panel wraps by width |
| Running footnote | "Alternated: this PR → main → this PR… · each round starts with 1000 warm-up calls that are not counted" | not drawn | no ruling draws it |
| Stop line | "main, round 3 of 5 · about 30 s left · finished rounds are kept" | "main, round 3 of 5 · finished rounds are kept" | no time estimate exists in the view |
| Empty state list | no button under *Load · comes later* | **+ Ask the copilot for a scenario** is drawn | the button is always in the list |
| Scenario center | title and path on one line, no **Remove** | **Remove** under the path; the description wraps in two lines | a scenario can be removed; the path is the card's right column |
| Scenario right panel | one suggestion card of one line | the suggestion card with its description and a **scenario** pill | the copilot draws the suggestion's text |
| Approval modal and Prepare main scrim | a dark scrim over the whole window | the app's scrim over the cockpit, with the page under it still readable | the app's modal scrim |
| Prepare main steps | a `☑` mark; recipe "sha 9c2e…41a" | the kit's checkbox; recipe "sha 9c2e…2d1" | the kit's checkbox; the demo shows the end of the recipe hash |
| Checkboxes, segments and pills | the mockup's drawn boxes, chips and pills | the kit's | one set of controls for the app |
| Entry | the Run Function tests mockup with the review's header | the same, with the author line, the checks and the demo's functions (`badge.swift`) | the fixture of the Functions sub-tab; the header is the review's |
| Dark theme | raised blocks and thin borders | the card's own surface and the kit's tints | the dark theme's surface steps |
