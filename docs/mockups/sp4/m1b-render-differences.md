# SP4 · M1b: how the renders differ from the mockups

The renders are `docs/assets/sp4-m1b-<scene>-{light,dark}.png`, made with
`clusia-app --demo --scene <name> [--light|--dark] --screenshot <png>`. The mockups are
`docs/mockups/sp4/png/{,Dark-}Tests-*.png`. These differences are known and accepted:

| Where | Mockup | Render | Why |
|---|---|---|---|
| Test list | "+ 33 more" footer | the list scrolls | the list is windowed over uniform rows |
| Failing test | `crates/store/src/refresh.rs:212` | `src/auth/refresh.rs:44` | the demo diff has this file and line, so Comment on this line lands in the diff |
| Failing test name in the detail | `failed_exchange_keeps_old_token` | the full `store::refresh::failed_exchange_keeps_old_token` | the detail shows the name the list cuts |
| Detail command | `cargo test -p store -- --exact …`, `running 1 test` | `cargo test -p store refresh` and the panic text | the demo run is a near-the-change run |
| Last run age | "4 min ago" | "4m ago" | the app's `format_age` |
| Reused | "image, cargo cache (1.2 GB)" | "image" | the demo's `Existing` holds an image and no cache folder |
| Run the tests buttons | full-width | as wide as their label, wrapping in a narrow card | the kit's wrapping button sizes to its label |
| Filters | the mockup's tinted pills (red Failed, green Near) | the kit's chips; only the selected one is tinted | the kit's chips |
| Failure mark | thin red `✕` | the kit's `×` | `✕` is not in the app's font and drew as a missing-glyph box |
| Failure detail block | two-colour output (`FAILED` in orange) | one colour | the output is drawn as plain text |
| Checks on GitHub card | across the bottom of the window | below the list and detail, reached by scrolling | the tab body scrolls at 1280 × 800 |
| Setup step titles | "Build the image from Dockerfile (target test)" and the like | the daemon's titles ("Build the image") | the window and `clusia tests --setup` say the same thing |
| Setup commands | cut with "…" | shown whole, wrapped | the exact command is the point of the modal |
| Setup footer | "About 1.4 GB to download" | no total | the daemon has no size |
| Setup review | `rzorzal/shop-demo #88`, npm and vitest | `rzorzal/clusia #123`, cargo | the demo review is fixed |
| Setup backdrop | a dark scrim | the page under a light veil | the app's modal scrim |
| Setup checkboxes | the mockup's boxes | the kit's checkbox | the kit's checkbox |
| Empty card | the Run Function tests lines | left out | Run Function tests comes later |
| `tests-functions` | the later sub-tab with cases | the sub-tabs and the note that it comes next | `Tests-functions.png` is the later mockup |
| Config › Tests | `~/.colima/default/docker.sock`; "5 of 10" and "4096 MB" inside the fields; the three cards across the row; the page without a subtitle; sections GitHub … Plugins | the full socket path; the unit beside the field; cards of fixed width; a subtitle; the repository's section order | the app's settings page |
| Config disk worktree outputs | "0.8 GB" | "800 MB" | `size_label` writes whole MB under 1 GB |
| Window chrome | traffic lights and a narrower tab bar | the app's tab bar, Config button and the review header with the author line | the render is the real window, the mockup is a drawing |
| Dark detail block | a raised block | the card's own surface | the dark theme's surface steps |
