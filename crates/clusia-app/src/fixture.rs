//! Demo data for `--demo` and screenshots: only `rzorzal` repositories and generic people.

use std::collections::BTreeMap;

use clusia_core::checks::{CheckKind, CheckResult, Finding, Pass, Severity, finding_id};
use clusia_core::diagram::{
    Change, CodeAnchor, DiagramEntry, DiagramKind, DiagramSpec, DiagramVersion, Edge, EntryState,
    Group, GroupKind, History, LineStyle, Message, Node, PlanEntry, Row, RowKey, Shape,
    VersionPhase,
};
use clusia_core::media::MediaKind;
use clusia_core::revalidation::{
    CommitLine, Decision, Proposal, Revalidation, RoundState, Verdict, fingerprint,
};
use clusia_core::time::civil_from_days;
use clusia_core::{
    ActivitySummary, Anchor, ChecksSummary, Config, DayCount, Draft, DraftItem, DraftKind,
    FileDiff, IssueComment, ItemStatus, Origin, PrConversation, PrDetail, PrRef, PrSummary, Review,
    ReviewInfo, ReviewState, ReviewThread, Role, Side, ThreadPost,
};
use clusia_protocol::{
    AgentLogEntry, AuthInfo, CheckState, CheckStatus, CodeLine, DiagramCode, DiagramsState,
    FileSummary, FirstRun, GifItem, GifPage, GithubLogin, Harness, HarnessKind, NewsItem, NewsKind,
    PermissionOutcome, PermissionRequest, PermissionStatus, ProbeResult, RepoFolder, ReviewSummary,
    ReviewView, Suggestion, SyncState, SyncStatus, TokenSource,
};

use clusia_view::diff::{RowKind, parse_patch};

use crate::bridge::{ChecksTell, DiagramsTell, Tell};
use crate::snapshot::{GiphyKey, Snapshot};

pub fn demo(now: i64) -> Snapshot {
    let pr =
        |repo: &str, number: u64, title: &str, author: &str, age: i64, draft: bool| PrSummary {
            pr: PrRef::new("rzorzal", repo, number).expect("valid demo ref"),
            title: title.into(),
            author: author.into(),
            url: format!("https://github.com/rzorzal/{repo}/pull/{number}"),
            draft,
            updated_at: rfc3339(now - age),
            comments: 2,
        };
    let review = |repo: &str, number: u64, title: &str, state, items, age: i64| ReviewSummary {
        pr: PrRef::new("rzorzal", repo, number).expect("valid demo ref"),
        title: title.into(),
        state,
        items,
        updated_at: now - age,
    };
    let day = 86_400;
    Snapshot {
        notifications_permission: PermissionStatus::Allowed,
        config: Config::default(),
        assigned: vec![
            pr("clusia", 123, "feat: auth refresh", "octo", 300, false),
            pr(
                "clusia",
                98,
                "api pagination for long pull request lists",
                "mona",
                day,
                false,
            ),
            pr("blog", 36, "Write the M3 post", "hubot", 3 * day, true),
            pr("site", 12, "Dark mode for the docs", "octo", 4 * day, false),
            pr(
                "clusia",
                61,
                "fix: sync backoff on rate limits",
                "mona",
                5 * day,
                false,
            ),
            pr("site", 14, "Fix the footer links", "hubot", 6 * day, false),
            pr(
                "clusia",
                57,
                "docs: config reference",
                "octo",
                8 * day,
                false,
            ),
        ],
        mine: vec![
            pr(
                "clusia",
                140,
                "feat(tray): menu bar popover",
                "rzorzal",
                600,
                false,
            ),
            pr(
                "clusia",
                131,
                "fix(git): never prune the user clone",
                "rzorzal",
                2 * day,
                false,
            ),
            pr(
                "blog",
                40,
                "New post: the autograph tree",
                "rzorzal",
                5 * day,
                true,
            ),
        ],
        reviews: vec![
            review(
                "clusia",
                77,
                "fix: cache invalidation",
                ReviewState::Outdated,
                2,
                3 * 3600,
            ),
            review("blog", 31, "New theme", ReviewState::Saved, 1, day),
        ],
        activity: Some(ActivitySummary {
            heatmap: heat(now),
            published_this_week: 12,
            published_total: 140,
            avg_review_secs: Some(45 * 60),
        }),
        sync: Some(SyncStatus {
            state: SyncState::Online,
            last_sync_unix: Some(now - 60),
            next_sync_unix: Some(now),
            ..SyncStatus::default()
        }),
        auth: Some(AuthInfo {
            source: Some(TokenSource::GhCli),
            login: Some("rzorzal".into()),
            scopes: vec!["repo".into(), "read:org".into()],
            error: None,
        }),
        giphy_key: GiphyKey::Set,
        first_run: None,
        first_run_open: false,
        lists_loaded: true,
        daemon_version: "demo".into(),
    }
}

/// What the first-run screen shows in demo mode (mockup `FirstRun.png`).
pub fn demo_first_run() -> FirstRun {
    let folder = |path: &str, exists, repos| RepoFolder {
        path: path.into(),
        exists,
        repos,
    };
    FirstRun {
        github: GithubLogin::SignedIn {
            login: "rzorzal".into(),
            scopes: vec!["repo".into(), "read:org".into()],
        },
        folders: vec![
            folder("~/Repos", true, 12),
            folder("~/Projects", true, 3),
            folder("~/src", true, 0),
            folder("~/code", false, 0),
        ],
        harnesses: vec![
            Harness {
                kind: HarnessKind::ClaudeCode,
                path: Some("/opt/homebrew/bin/claude".into()),
                version: Some("2.1.0".into()),
            },
            Harness {
                kind: HarnessKind::Codex,
                path: None,
                version: None,
            },
        ],
    }
}

/// `(id, title)` of the GIFs the demo's Giphy search knows.
const DEMO_GIFS: [(&str, &str); 6] = [
    ("demo-party", "party turtle"),
    ("demo-thumbs", "thumbs up"),
    ("demo-mind", "mind blown"),
    ("demo-clap", "applause"),
    ("demo-ship", "ship it"),
    ("demo-coffee", "coffee time"),
];

/// The demo's answer to a Giphy search: every GIF for an empty query, else the titles that
/// contain it (ignoring case). Only the first page has results.
pub fn demo_gif_page(query: &str, offset: u32) -> GifPage {
    let query = query.trim().to_lowercase();
    let items = DEMO_GIFS
        .iter()
        .filter(|_| offset == 0)
        .filter(|(_, title)| title.contains(&query))
        .map(|(id, title)| GifItem {
            id: (*id).into(),
            title: (*title).into(),
            preview_url: format!("https://media.giphy.com/media/{id}/100w.gif"),
            url: format!("https://media.giphy.com/media/{id}/giphy.gif"),
            width: 100,
            height: 75,
        })
        .collect();
    GifPage {
        items,
        next_offset: None,
    }
}

/// A small looping animation colored by `url`, standing in for a downloaded GIF.
pub fn demo_gif_bytes(url: &str) -> Vec<u8> {
    use image::codecs::gif::{GifEncoder, Repeat};
    use image::{Delay, Frame, Rgba, RgbaImage};
    const WIDTH: u32 = 120;
    const HEIGHT: u32 = 90;
    const SIDE: u32 = 24;
    const FRAMES: u32 = 6;
    let hash = url.bytes().fold(2_166_136_261_u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(16_777_619)
    });
    let channel = |shift: u32| 80 + ((hash >> shift) & 0x7f) as u8;
    let ink = Rgba([channel(0), channel(8), channel(16), 255]);
    let mut out = Vec::new();
    {
        let mut encoder = GifEncoder::new(&mut out);
        encoder
            .set_repeat(Repeat::Infinite)
            .expect("an in-memory encoder takes the repeat");
        for i in 0..FRAMES {
            let mut image = RgbaImage::from_pixel(WIDTH, HEIGHT, Rgba([240, 244, 240, 255]));
            let x = 8 + i * (WIDTH - SIDE - 16) / (FRAMES - 1);
            for dy in 0..SIDE {
                for dx in 0..SIDE {
                    image.put_pixel(x + dx, 33 + dy, ink);
                }
            }
            let frame = Frame::from_parts(image, 0, 0, Delay::from_numer_denom_ms(120, 1));
            encoder
                .encode_frame(frame)
                .expect("an in-memory encoder takes the frame");
        }
    }
    out
}

/// The demo's copy of a picture: Giphy URLs are animated GIFs, nothing else exists.
pub fn demo_media(url: &str) -> Option<(MediaKind, Vec<u8>)> {
    url.starts_with("https://media.giphy.com/")
        .then(|| (MediaKind::Gif, demo_gif_bytes(url)))
}

/// The review every demo scene shows (mockup `Review.png`).
pub fn demo_pr() -> PrRef {
    PrRef::new("rzorzal", "clusia", 123).expect("valid demo ref")
}

/// The code a demo diagram box shows: the lines of `patch`'s new side from three above `lines`
/// to three below, the ones the patch adds marked changed when the drawing is of the head. A
/// file the patch does not cover is "no longer available", as the daemon says it.
pub fn demo_diagram_code(
    path: &str,
    commit: &str,
    patch: Option<&str>,
    lines: (u32, u32),
    at_head: bool,
) -> DiagramCode {
    // As the daemon reads it: from the box's first line to two lines past its last, with only
    // the box's own added lines tinted.
    let from = lines.0.max(1);
    let to = lines.1.saturating_add(2);
    let shown: Vec<CodeLine> = patch
        .map(parse_patch)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|row| {
            let number = row.new?;
            (row.kind != RowKind::Removed && (from..=to).contains(&number)).then(|| CodeLine {
                number,
                text: row.text,
                changed: at_head && row.kind == RowKind::Added && number <= lines.1,
            })
        })
        .take(200)
        .collect();
    DiagramCode {
        path: path.into(),
        commit: commit.into(),
        start_line: shown.first().map_or(lines.0, |l| l.number),
        unavailable: shown.is_empty().then(|| {
            format!(
                "The code at {} is no longer available.",
                commit.get(..7).unwrap_or(commit)
            )
        }),
        lines: shown,
    }
}

const BASE_SHA: &str = "4f2a9c1e8d7b6a5f4e3d2c1b0a9f8e7d6c5b4a39";
const HEAD_SHA: &str = "9be1d07c3a2f4e5d6c7b8a9f0e1d2c3b4a5f6e7d";

/// `rzorzal/clusia#123 feat: auth refresh` by `octo`, opened by `rzorzal` as a reviewer: seven
/// files (+120 −34) with real patches in `view.diff`, checks passing, a conversation (one open
/// thread on `src/auth/refresh.rs:41`, one resolved thread, hubot's approval, two comments),
/// a draft of three items (ok, moved, obsolete), and the What's new rows of `WhatsNew.png`.
pub fn demo_review(now: i64) -> (ReviewView, Vec<NewsItem>) {
    let pr = demo_pr();
    let diff = demo_diff();
    let title = "feat: auth refresh";
    let detail = PrDetail {
        summary: PrSummary {
            pr: pr.clone(),
            title: title.into(),
            author: "octo".into(),
            url: "https://github.com/rzorzal/clusia/pull/123".into(),
            draft: false,
            updated_at: rfc3339(now - 300),
            comments: 5,
        },
        base_ref: "main".into(),
        head_ref: "octo:auth-refresh".into(),
        base_sha: BASE_SHA.into(),
        head_sha: HEAD_SHA.into(),
        additions: diff.iter().map(|f| f.additions).sum(),
        deletions: diff.iter().map(|f| f.deletions).sum(),
        changed_files: diff.len() as u64,
        clone_url: "https://github.com/rzorzal/clusia.git".into(),
        closed: false,
        merged: false,
        body: "Refreshes the access token a minute before it expires, so a long review never meets an expired one.".into(),
    };
    let mut review = Review::new(
        pr.clone(),
        title.into(),
        BASE_SHA.into(),
        HEAD_SHA.into(),
        now - 2 * 86_400,
    );
    review.updated_at = now - 600;
    review.last_seen_at = Some(now - 20 * 3600);
    review.last_checks = Some("pending".into());
    review.draft = demo_draft(now);
    let view = ReviewView {
        review,
        pr: detail,
        files: diff.iter().map(FileSummary::from).collect(),
        role: Role::Reviewer,
        worktree: Some("~/.clusia/worktrees/rzorzal/clusia/123".into()),
        viewer: Some("rzorzal".into()),
        checks: Some(ChecksSummary {
            total: 3,
            passed: 3,
            failed: 0,
            pending: 0,
        }),
        conversation: Some(demo_conversation(now)),
        diff,
    };
    (view, demo_news(now))
}

fn demo_draft(now: i64) -> Draft {
    let item = |id: &str, path: &str, line: u32, body: &str, status: ItemStatus| DraftItem {
        id: id.into(),
        kind: DraftKind::LineComment,
        origin: Origin::Human,
        anchor: Some(Anchor {
            path: path.into(),
            line,
            start_line: None,
            side: Side::Right,
            commit: HEAD_SHA.into(),
        }),
        body: body.into(),
        status,
        accepted: true,
        created_at: now - 3600,
        thread: None,
    };
    Draft {
        items: vec![
            item(
                "i1",
                "src/auth/refresh.rs",
                44,
                "Holding the lock across the network call means a slow exchange blocks every \
                 request. Could we re-check the expiry after taking the lock instead, and drop \
                 it before `exchange`?",
                ItemStatus::Ok,
            ),
            item(
                "i2",
                "src/auth/store.rs",
                88,
                "save() should write to a temp file and rename, so a crash never leaves half a \
                 token on disk.",
                ItemStatus::Moved {
                    from_path: "src/auth/store.rs".into(),
                    from_line: 81,
                },
            ),
            item(
                "i3",
                "src/client/http.rs",
                20,
                "This retry loop is gone in the new version. Remove or re-add this comment on \
                 the new code.",
                ItemStatus::Obsolete {
                    reason: "the line changed".into(),
                },
            ),
        ],
        next_id: 3,
    }
}

fn demo_conversation(now: i64) -> PrConversation {
    let url = |id: u64| format!("https://github.com/rzorzal/clusia/pull/123#discussion_r{id}");
    let post = |id: u64, author: &str, body: &str, age: i64| ThreadPost {
        database_id: Some(id),
        author: author.into(),
        body: body.into(),
        created_at: rfc3339(now - age),
        url: url(id),
    };
    PrConversation {
        threads: Vec::new(),
        comments: vec![
            IssueComment {
                id: 2001,
                author: "joao".into(),
                body: "Tested against the staging token server: the **refresh works** :white_check_mark:\n\nThe retry waits `30s` before it asks again, as the CLI does:\n\n```rust\nlet delay = Duration::from_secs(30);\n```".into(),
                created_at: rfc3339(now - 5 * 3600),
                url: "https://github.com/rzorzal/clusia/pull/123#issuecomment-2001".into(),
            },
            IssueComment {
                id: 2002,
                author: "octo".into(),
                body: "Thanks! _Rebased on main_ :tada: Here is the run on my machine:\n\n![party](https://media.giphy.com/media/demo-party/giphy.gif)\n\nNotes in the [auth guide](https://github.com/rzorzal/clusia/pull/98) 🐢".into(),
                created_at: rfc3339(now - 4 * 3600),
                url: "https://github.com/rzorzal/clusia/pull/123#issuecomment-2002".into(),
            },
        ],
        reviews: vec![ReviewInfo {
            id: 3001,
            author: "hubot".into(),
            state: "APPROVED".into(),
            body: "Looks good once the lock change lands.".into(),
            submitted_at: Some(rfc3339(now - 3 * 3600)),
            url: "https://github.com/rzorzal/clusia/pull/123#pullrequestreview-3001".into(),
            commit_id: None,
        }],
        review_threads: vec![
            ReviewThread {
                id: "PRRT_demo_refresh_41".into(),
                is_resolved: false,
                is_outdated: false,
                path: "src/auth/refresh.rs".into(),
                line: Some(41),
                start_line: None,
                side: Side::Right,
                viewer_can_reply: true,
                viewer_can_resolve: true,
                comments: vec![
                    post(
                        1001,
                        "mona",
                        "Why one minute? The CLI uses 30 seconds.",
                        6 * 3600,
                    ),
                    post(
                        1002,
                        "octo",
                        "Clock skew on the CI runners. 30 seconds would work too.",
                        2 * 3600,
                    ),
                    post(
                        1005,
                        "hubot",
                        "A minute is safer while the runners drift.",
                        3600,
                    ),
                ],
            },
            ReviewThread {
                id: "PRRT_demo_mod_3".into(),
                is_resolved: true,
                is_outdated: false,
                path: "src/auth/mod.rs".into(),
                line: Some(3),
                start_line: None,
                side: Side::Right,
                viewer_can_reply: true,
                viewer_can_resolve: true,
                comments: vec![
                    post(1003, "ana", "Does the store need to be public?", 26 * 3600),
                    post(1004, "octo", "No: it is pub(crate) now.", 25 * 3600),
                ],
            },
        ],
    }
}

/// What the demo agent suggests (mockup `AgentChat.png`).
pub fn demo_suggestion() -> Suggestion {
    Suggestion {
        id: "sug-5f0c1d2e3a4b".into(),
        file: "src/auth/refresh.rs".into(),
        line: Some(44),
        start_line: None,
        end_line: None,
        body: "Re-check `expires_at` after taking the lock, so callers that waited reuse the token the first one fetched.".into(),
    }
}

/// What the demo agent asks to run (mockup `Permission.png`): 1:52 left when it is asked.
pub fn demo_permission_request(now_ms: i64) -> PermissionRequest {
    PermissionRequest {
        id: "perm-7c1e5a90".into(),
        pr: demo_pr(),
        turn: 3,
        tool: "Bash".into(),
        summary: "cargo test -p clusia-auth refresh_race -- --nocapture".into(),
        reason: Some("To check that two refreshes at once don't exchange the token twice.".into()),
        prefix: Some("cargo test".into()),
        sandbox: true,
        deadline: now_ms + 112_000,
        detail: None,
        origin: clusia_protocol::TurnOrigin::Chat,
    }
}

/// The rule "Allow for this review" grants in the demo.
pub fn demo_permission_rules() -> Vec<String> {
    vec!["Bash(cargo test:*)".into()]
}

/// Every suggestion the demo agent can make (`--demo` accepts them by id).
pub fn demo_suggestions() -> Vec<Suggestion> {
    vec![demo_suggestion()]
}

/// A good harness test: Claude Code answered in 1.8 s.
pub fn demo_probe() -> ProbeResult {
    ProbeResult {
        ok: true,
        version: Some("2.1.294".into()),
        program: "/opt/homebrew/bin/claude".into(),
        elapsed_ms: 1800,
        error: None,
        harness: "Claude Code".into(),
        signed_in: None,
        account: None,
    }
}

/// The settings that make the demo use Codex.
pub const CODEX_DEMO_SETTINGS: [(&str, &str); 2] = [
    ("harness.kind", "codex"),
    ("harness.program", "/opt/homebrew/bin/codex"),
];

/// The demo with Codex chosen as the agent.
pub fn demo_codex(now: i64) -> Snapshot {
    let mut snap = demo(now);
    for (key, value) in CODEX_DEMO_SETTINGS {
        crate::snapshot::apply_config_locally(&mut snap.config, key, value)
            .expect("the demo setting is valid");
    }
    snap
}

/// A good Codex test: it answered in 0.4 s and is signed in with ChatGPT.
pub fn demo_codex_probe() -> ProbeResult {
    ProbeResult {
        ok: true,
        version: Some("0.162.1".into()),
        program: "/opt/homebrew/bin/codex".into(),
        elapsed_ms: 400,
        error: None,
        harness: "Codex".into(),
        signed_in: Some(true),
        account: Some("Logged in using ChatGPT".into()),
    }
}

/// The settings that make the demo use a custom command called Echo. The program is only a
/// string to show: the demo never starts it.
pub const CUSTOM_DEMO_SETTINGS: [(&str, &str); 3] = [
    ("harness.kind", "custom"),
    ("harness.program", "/usr/local/bin/echo-harness"),
    ("harness.custom_name", "Echo"),
];

/// The demo with a custom command chosen as the agent.
pub fn demo_custom(now: i64) -> Snapshot {
    let mut snap = demo(now);
    for (key, value) in CUSTOM_DEMO_SETTINGS {
        crate::snapshot::apply_config_locally(&mut snap.config, key, value)
            .expect("the demo setting is valid");
    }
    snap
}

/// A good custom-command test: Echo answered the probe in 0.2 s.
pub fn demo_custom_probe() -> ProbeResult {
    ProbeResult {
        ok: true,
        version: Some("1.0".into()),
        program: "/usr/local/bin/echo-harness".into(),
        elapsed_ms: 200,
        error: None,
        harness: "Echo".into(),
        signed_in: None,
        account: None,
    }
}

/// A request of kind `other`: it names no command or path, so it has no rule to grant.
pub fn demo_permission_other(now_ms: i64) -> PermissionRequest {
    PermissionRequest {
        id: "perm-other-1".into(),
        pr: demo_pr(),
        turn: 3,
        tool: "Other".into(),
        summary: "Remove the build cache".into(),
        reason: Some("To rebuild from a clean state before running the tests.".into()),
        prefix: None,
        sandbox: false,
        deadline: now_ms + 112_000,
        detail: None,
        origin: clusia_protocol::TurnOrigin::Chat,
    }
}

/// The first-run scan when both harnesses are installed.
pub fn demo_first_run_both() -> FirstRun {
    let mut first_run = demo_first_run();
    for h in &mut first_run.harnesses {
        if h.kind == HarnessKind::Codex {
            h.path = Some("/opt/homebrew/bin/codex".into());
            h.version = Some("0.162.1".into());
        }
    }
    first_run
}

/// The demo review's chat, one inner list per turn: the summary the agent wrote when the
/// review opened (with a denied command), then the question and answer of `AgentChat.png`.
pub fn demo_agent_turns(now: i64) -> Vec<Vec<AgentLogEntry>> {
    let at = |ago: i64| now - ago;
    let first = vec![
        AgentLogEntry::ToolUse {
            at: at(3_600),
            turn: 1,
            summary: "Read src/auth/refresh.rs".into(),
        },
        AgentLogEntry::Permission {
            at: at(3_599),
            turn: 1,
            tool: "Bash".into(),
            summary: "cargo test".into(),
            outcome: PermissionOutcome::Denied,
        },
        AgentLogEntry::Text {
            at: at(3_598),
            turn: 1,
            text: "**Summary.** This pull request makes `TokenStore::refresh` safe to call from several tasks at once.\n\n- the token is refreshed one minute before it expires\n- a lock stops two callers from exchanging the same refresh token\n\nI could not run the tests: you denied the command.".into(),
        },
        AgentLogEntry::Done {
            at: at(3_596),
            turn: 1,
            duration_ms: 4_200,
        },
    ];
    let second = vec![
        AgentLogEntry::User {
            at: at(120),
            turn: 2,
            text: "Is the new lock needed at all, or would re-checking the expiry be enough?".into(),
        },
        AgentLogEntry::ToolUse {
            at: at(119),
            turn: 2,
            summary: "Read src/auth/store.rs and client/http.rs".into(),
        },
        AgentLogEntry::ToolUse {
            at: at(118),
            turn: 2,
            summary: "Searched for refresh_lock · 3 uses".into(),
        },
        AgentLogEntry::Text {
            at: at(117),
            turn: 2,
            text: "The lock is needed: two requests that both see an expired token would otherwise exchange the same refresh token twice, and GitHub rejects the second one.\n\nWhat's not needed is holding it during the network call. Re-check the expiry right after taking the lock; most callers will find a fresh token and leave at once.".into(),
        },
        AgentLogEntry::Suggestion {
            at: at(116),
            turn: 2,
            suggestion: demo_suggestion(),
        },
        AgentLogEntry::Done {
            at: at(115),
            turn: 2,
            duration_ms: 1_800,
        },
    ];
    vec![first, second]
}

/// The whole demo chat, oldest first, as the daemon's log would give it.
pub fn demo_agent_log(now: i64) -> Vec<AgentLogEntry> {
    demo_agent_turns(now).concat()
}

/// The ids of the findings `draft` already holds: an item a check wrote (origin Security or
/// Audit) on the finding's file and line.
pub fn demo_accepted(results: &[CheckResult], draft: &Draft) -> Vec<String> {
    results
        .iter()
        .flat_map(|r| &r.findings)
        .filter(|f| {
            draft.items.iter().any(|i| {
                matches!(i.origin, Origin::Security | Origin::Audit)
                    && i.anchor
                        .as_ref()
                        .is_some_and(|a| a.path == f.file && Some(a.line) == f.line)
            })
        })
        .map(|f| f.id.clone())
        .collect()
}

/// `demo_checks` as the tell a scene sends through the outbox. The findings `draft` already
/// holds are accepted; nothing is dismissed.
pub fn demo_checks_tell(pr: &PrRef, now: i64, draft: &Draft) -> Tell {
    let (results, states) = demo_checks(now);
    let accepted = demo_accepted(&results, draft);
    Tell::Checks(ChecksTell::Loaded {
        pr: pr.clone(),
        results,
        states,
        accepted,
        dismissed: Vec::new(),
    })
}

const DIAGRAMS_V1_COMMIT: &str = "3f2a1c0e8d7b6a5f4e3d2c1b0a9f8e7d6c5b4a39";

fn dg_node(id: &str, label: &str, sub: Option<&str>, shape: Shape, change: Change) -> Node {
    Node {
        id: id.into(),
        label: label.into(),
        sub: sub.map(Into::into),
        shape,
        rows: Vec::new(),
        divider_after: None,
        anchor: None,
        change,
        note: None,
        history: None,
    }
}

fn dg_at(mut node: Node, path: &str, start_line: u32, end_line: u32) -> Node {
    node.anchor = Some(CodeAnchor {
        path: path.into(),
        start_line,
        end_line,
    });
    node
}

fn dg_rows(mut node: Node, rows: &[(&str, Option<RowKey>, Option<Change>)]) -> Node {
    node.rows = rows
        .iter()
        .map(|(text, key, change)| Row {
            text: (*text).into(),
            key: *key,
            change: *change,
        })
        .collect();
    node
}

fn dg_edge(from: &str, to: &str, label: Option<&str>, change: Change) -> Edge {
    Edge {
        from: from.into(),
        to: to.into(),
        label: label.map(Into::into),
        style: LineStyle::Solid,
        ends: [None, None],
        change,
        history: None,
    }
}

fn dg_ends(mut edge: Edge, from: &str, to: &str) -> Edge {
    edge.ends = [Some(from.into()), Some(to.into())];
    edge
}

fn dg_message(from: &str, to: &str, label: &str, reply: bool, change: Change) -> Message {
    Message {
        from: from.into(),
        to: to.into(),
        label: label.into(),
        reply,
        change,
        history: None,
    }
}

fn dg_spec(key: &str, kind: DiagramKind, title: &str, caption: &str) -> DiagramSpec {
    DiagramSpec {
        key: key.into(),
        kind,
        title: title.into(),
        caption: caption.into(),
        nodes: Vec::new(),
        edges: Vec::new(),
        groups: Vec::new(),
        messages: Vec::new(),
    }
}

/// Marks every node, edge and message of `spec` as `Same`, then applies `marks`: the ids of
/// nodes (`"id"`), edges (`"from>to"`) or messages (`"#n"`, one-based) and what changed.
fn dg_history(mut spec: DiagramSpec, marks: &[(&str, History)]) -> DiagramSpec {
    let mark = |name: &str| {
        marks
            .iter()
            .find(|(n, _)| *n == name)
            .map_or(History::Same, |m| m.1)
    };
    for node in &mut spec.nodes {
        node.history = Some(mark(&node.id));
    }
    for edge in &mut spec.edges {
        edge.history = Some(mark(&format!("{}>{}", edge.from, edge.to)));
    }
    for (i, message) in spec.messages.iter_mut().enumerate() {
        message.history = Some(mark(&format!("#{}", i + 1)));
    }
    spec
}

fn dg_flow() -> DiagramSpec {
    let mut s = dg_spec(
        "flow",
        DiagramKind::Flow,
        "Token refresh flow",
        "Click a box to comment on its code",
    );
    s.nodes = vec![
        dg_at(
            dg_node(
                "http",
                "HTTP client",
                Some("client/http.rs"),
                Shape::Box,
                Change::Unchanged,
            ),
            "src/client/http.rs",
            12,
            36,
        ),
        dg_at(
            dg_node(
                "refresh",
                "TokenStore::refresh",
                Some("+9 −2 · 1 draft comment"),
                Shape::Box,
                Change::Changed,
            ),
            "src/auth/refresh.rs",
            38,
            48,
        ),
        dg_node(
            "oauth",
            "OAuth token endpoint",
            Some("github.com · external"),
            Shape::External,
            Change::Unchanged,
        ),
        dg_at(
            dg_node(
                "lock",
                "refresh_lock",
                Some("Mutex<()> · new"),
                Shape::Box,
                Change::New,
            ),
            "src/auth/store.rs",
            12,
            14,
        ),
        dg_at(
            dg_node(
                "save",
                "TokenStore::save",
                Some("Risk: not atomic"),
                Shape::Box,
                Change::Risk,
            ),
            "src/auth/store.rs",
            40,
            44,
        ),
    ];
    s.edges = vec![
        dg_edge("http", "refresh", Some("token()"), Change::Unchanged),
        dg_edge("refresh", "oauth", Some("exchange()"), Change::Unchanged),
        dg_edge("refresh", "lock", Some("new"), Change::New),
        dg_edge("oauth", "save", None, Change::Unchanged),
        dg_edge("http", "save", Some("save()"), Change::Unchanged),
    ];
    dg_history(
        s,
        &[
            ("refresh", History::Changed),
            ("lock", History::Added),
            ("refresh>lock", History::Added),
            ("http>save", History::Removed),
        ],
    )
}

fn dg_deps() -> DiagramSpec {
    let mut s = dg_spec(
        "deps",
        DiagramKind::Graph,
        "Module dependencies",
        "Arrows point to the module being used",
    );
    s.nodes = vec![
        dg_node(
            "cli",
            "cli",
            Some("unchanged"),
            Shape::Box,
            Change::Unchanged,
        ),
        dg_node(
            "handlers",
            "clusiad::handlers",
            Some("unchanged"),
            Shape::Box,
            Change::Unchanged,
        ),
        dg_node(
            "auth",
            "auth",
            Some("+53 −13 · 3 files"),
            Shape::Box,
            Change::Changed,
        ),
        dg_node(
            "client",
            "client",
            Some("+28 −9 · retries"),
            Shape::Box,
            Change::Changed,
        ),
        dg_node(
            "config",
            "config",
            Some("+6 · token margin"),
            Shape::Box,
            Change::Changed,
        ),
        dg_node(
            "tracing",
            "tracing",
            Some("external crate"),
            Shape::External,
            Change::Unchanged,
        ),
    ];
    s.edges = vec![
        dg_edge("cli", "auth", None, Change::Unchanged),
        dg_edge("handlers", "auth", None, Change::Unchanged),
        dg_edge("handlers", "client", None, Change::Unchanged),
        dg_edge("auth", "client", Some("new dependency"), Change::New),
        dg_edge(
            "auth",
            "config",
            Some("reads refresh margin"),
            Change::Unchanged,
        ),
        dg_edge(
            "client",
            "tracing",
            Some("logs requests"),
            Change::Unchanged,
        ),
    ];
    s
}

fn dg_sequence() -> DiagramSpec {
    let mut s = dg_spec(
        "sequence",
        DiagramKind::Sequence,
        "Refresh call, step by step",
        "Two requests arrive while the token is about to expire",
    );
    s.nodes = vec![
        dg_node("a", "Request A", None, Shape::Actor, Change::Unchanged),
        dg_node("b", "Request B", None, Shape::Actor, Change::Unchanged),
        dg_at(
            dg_node("store", "TokenStore", None, Shape::Box, Change::Changed),
            "src/auth/refresh.rs",
            38,
            48,
        ),
        dg_at(
            dg_node("lock", "refresh_lock", None, Shape::Box, Change::New),
            "src/auth/store.rs",
            12,
            14,
        ),
        dg_node(
            "oauth",
            "OAuth endpoint",
            None,
            Shape::Box,
            Change::Unchanged,
        ),
    ];
    s.messages = vec![
        dg_message("a", "store", "token()", false, Change::Unchanged),
        dg_message("store", "lock", "lock()", false, Change::New),
        dg_message("b", "store", "token()", false, Change::Unchanged),
        dg_message("store", "lock", "lock() · waits for A", false, Change::Risk),
        dg_message(
            "store",
            "oauth",
            "exchange(refresh)",
            false,
            Change::Unchanged,
        ),
        dg_message("oauth", "store", "new token", true, Change::Unchanged),
        dg_message("store", "a", "Ok(token)", true, Change::Unchanged),
        dg_message(
            "store",
            "b",
            "Ok(same token) · no second exchange",
            true,
            Change::New,
        ),
    ];
    dg_history(
        s,
        &[
            ("store", History::Changed),
            ("lock", History::Added),
            ("#2", History::Added),
            ("#4", History::Added),
            ("#8", History::Changed),
        ],
    )
}

fn dg_er() -> DiagramSpec {
    let pk = Some(RowKey::Pk);
    let fk = Some(RowKey::Fk);
    let mut s = dg_spec(
        "er",
        DiagramKind::EntityRelationship,
        "Token data model",
        "What is stored and how it relates",
    );
    s.nodes = vec![
        dg_rows(
            dg_node("account", "Account", None, Shape::Table, Change::Unchanged),
            &[
                ("id", pk, None),
                ("login", None, None),
                ("host", None, None),
            ],
        ),
        dg_rows(
            dg_node(
                "token",
                "Token",
                Some("changed"),
                Shape::Table,
                Change::Changed,
            ),
            &[
                ("id", pk, None),
                ("access_token", None, None),
                ("refresh_token", None, None),
                ("expires_at", None, None),
                ("account_id", fk, None),
                ("refreshed_at · new", None, Some(Change::New)),
            ],
        ),
        dg_rows(
            dg_node(
                "attempt",
                "RefreshAttempt",
                Some("new table"),
                Shape::Table,
                Change::New,
            ),
            &[
                ("id", pk, None),
                ("token_id", fk, None),
                ("started_at", None, None),
                ("outcome", None, None),
            ],
        ),
        dg_rows(
            dg_node(
                "file",
                "TokenFile",
                Some("risk: permissions"),
                Shape::Table,
                Change::Risk,
            ),
            &[
                ("path", None, None),
                ("mode  0644 → 0600", None, Some(Change::Risk)),
                ("written_at", None, None),
            ],
        ),
    ];
    s.edges = vec![
        dg_ends(
            dg_edge("account", "token", Some("has"), Change::Unchanged),
            "1",
            "0..N",
        ),
        dg_ends(
            dg_edge("token", "attempt", Some("records"), Change::New),
            "1",
            "0..N",
        ),
        dg_ends(
            dg_edge("token", "file", Some("saved as"), Change::Unchanged),
            "1",
            "1",
        ),
    ];
    s
}

fn dg_usecase() -> DiagramSpec {
    let pill = |id: &str, label: &str, sub: Option<&str>, change| {
        dg_node(id, label, sub, Shape::Pill, change)
    };
    let mut s = dg_spec(
        "usecase",
        DiagramKind::UseCases,
        "What people can do",
        "Actors on the sides, Clúsia in the middle",
    );
    s.nodes = vec![
        dg_node(
            "reviewer",
            "«actor» Reviewer",
            None,
            Shape::Actor,
            Change::Unchanged,
        ),
        dg_node(
            "agent",
            "«actor» Agent",
            Some("Claude Code"),
            Shape::Actor,
            Change::Unchanged,
        ),
        dg_node(
            "github",
            "«system» GitHub",
            None,
            Shape::External,
            Change::Unchanged,
        ),
        pill("open", "Open a review", None, Change::Unchanged),
        pill("news", "See what's new", None, Change::Unchanged),
        pill("comment", "Comment on code", None, Change::Unchanged),
        pill("ask", "Ask the agent", None, Change::Unchanged),
        pill("publish", "Publish the review", None, Change::Unchanged),
        pill(
            "token",
            "Keep the GitHub token fresh",
            Some("this PR"),
            Change::Changed,
        ),
        pill(
            "recheck",
            "Re-check after new commits",
            None,
            Change::Unchanged,
        ),
    ];
    s.groups = vec![Group {
        id: "clusia".into(),
        label: "Clúsia".into(),
        members: [
            "open", "news", "comment", "ask", "publish", "token", "recheck",
        ]
        .map(String::from)
        .to_vec(),
        kind: GroupKind::Boundary,
    }];
    // Links are plain lines; each points at the actor on the right so that actor sits there.
    s.edges = vec![
        dg_edge("reviewer", "open", None, Change::Unchanged),
        dg_edge("reviewer", "comment", None, Change::Unchanged),
        dg_edge("reviewer", "publish", None, Change::Unchanged),
        dg_edge("ask", "agent", None, Change::Unchanged),
        dg_edge("token", "github", None, Change::Unchanged),
        dg_edge("recheck", "github", None, Change::Unchanged),
    ];
    s
}

fn dg_dataflow() -> DiagramSpec {
    let mut s = dg_spec(
        "dataflow",
        DiagramKind::DataFlow,
        "Where the token goes",
        "Circles transform data, open boxes store it",
    );
    s.nodes = vec![
        dg_node(
            "client",
            "API client",
            Some("external"),
            Shape::External,
            Change::Unchanged,
        ),
        dg_node(
            "refresh",
            "1 Refresh",
            Some("margin 60 s"),
            Shape::Circle,
            Change::Changed,
        ),
        dg_node(
            "exchange",
            "2 Exchange",
            Some("retries"),
            Shape::Circle,
            Change::Changed,
        ),
        dg_node(
            "oauth",
            "GitHub OAuth",
            Some("external"),
            Shape::External,
            Change::Unchanged,
        ),
        dg_node(
            "file",
            "D1  |  Token file",
            Some("written atomically"),
            Shape::Store,
            Change::Unchanged,
        ),
    ];
    s.edges = vec![
        dg_edge(
            "client",
            "refresh",
            Some("token request"),
            Change::Unchanged,
        ),
        dg_edge(
            "refresh",
            "exchange",
            Some("refresh token"),
            Change::Unchanged,
        ),
        dg_edge(
            "exchange",
            "oauth",
            Some("grant request"),
            Change::Unchanged,
        ),
        dg_edge(
            "oauth",
            "exchange",
            Some("access + refresh token"),
            Change::Unchanged,
        ),
        dg_edge(
            "exchange",
            "file",
            Some("write (temp file + rename)"),
            Change::New,
        ),
        dg_edge("file", "refresh", Some("read"), Change::Unchanged),
    ];
    s
}

fn dg_states() -> DiagramSpec {
    let mut s = dg_spec(
        "states",
        DiagramKind::States,
        "Token life cycle",
        "Each arrow is the event that moves the token on",
    );
    s.nodes = vec![
        dg_node("start", "Start", None, Shape::Start, Change::Unchanged),
        dg_node("fresh", "Fresh", None, Shape::Pill, Change::Unchanged),
        dg_node(
            "expiring",
            "Expiring",
            Some("within 60 s · new"),
            Shape::Pill,
            Change::New,
        ),
        dg_node(
            "refreshing",
            "Refreshing",
            Some("one caller, others wait"),
            Shape::Pill,
            Change::Changed,
        ),
        dg_node(
            "failed",
            "Failed",
            Some("retries: max 3"),
            Shape::Pill,
            Change::Risk,
        ),
    ];
    s.edges = vec![
        dg_edge("start", "fresh", None, Change::Unchanged),
        dg_edge("fresh", "expiring", Some("time passes"), Change::Unchanged),
        dg_edge(
            "expiring",
            "refreshing",
            Some("first caller locks"),
            Change::Unchanged,
        ),
        dg_edge(
            "refreshing",
            "fresh",
            Some("new token saved"),
            Change::Unchanged,
        ),
        dg_edge(
            "refreshing",
            "failed",
            Some("exchange failed"),
            Change::Risk,
        ),
        dg_edge("failed", "expiring", Some("retry"), Change::Risk),
    ];
    s
}

fn dg_classes() -> DiagramSpec {
    let new = Some(Change::New);
    let changed = Some(Change::Changed);
    let mut s = dg_spec(
        "classes",
        DiagramKind::Classes,
        "Types involved",
        "Fields on top, methods below",
    );
    let mut token = dg_rows(
        dg_node("token", "Token", None, Shape::Class, Change::Unchanged),
        &[
            ("access: String", None, None),
            ("refresh: String", None, None),
            ("expires_at: Instant", None, None),
            ("is_fresh(margin) · new", None, new),
        ],
    );
    token.divider_after = Some(3);
    let mut store = dg_rows(
        dg_node("store", "TokenStore", None, Shape::Class, Change::Changed),
        &[
            ("path: PathBuf", None, None),
            ("client: HttpClient · new", None, new),
            ("refresh_lock: Mutex<()> · new", None, new),
            ("token() -> Token", None, None),
            ("refresh() · changed", None, changed),
            ("save(&Token) · changed", None, changed),
            ("load() -> Token", None, None),
        ],
    );
    store.divider_after = Some(3);
    let mut client = dg_rows(
        dg_node("client", "HttpClient", None, Shape::Class, Change::Changed),
        &[
            ("base: Url", None, None),
            ("retries: u8 · new", None, new),
            ("exchange(&str) · changed", None, changed),
            ("send(&Request)", None, None),
        ],
    );
    client.divider_after = Some(2);
    s.nodes = vec![
        dg_at(token, "src/auth/mod.rs", 8, 24),
        dg_at(store, "src/auth/store.rs", 10, 60),
        dg_at(client, "src/client/http.rs", 8, 40),
    ];
    s.edges = vec![
        dg_edge("store", "token", Some("returns"), Change::Unchanged),
        dg_edge("store", "client", Some("owns 1"), Change::Unchanged),
    ];
    dg_history(
        s,
        &[
            ("store", History::Changed),
            ("client", History::Changed),
            ("store>client", History::Added),
        ],
    )
}

/// What the first drawing (version 1, at an older commit) showed of `spec`: nothing that the
/// second drawing marks as added, and no marks.
fn dg_before(spec: &DiagramSpec) -> DiagramSpec {
    let gone = |h: Option<History>| h == Some(History::Added);
    let mut before = spec.clone();
    before.nodes.retain(|n| !gone(n.history));
    before.edges.retain(|e| !gone(e.history));
    before.messages.retain(|m| !gone(m.history));
    for node in &mut before.nodes {
        node.history = None;
    }
    for edge in &mut before.edges {
        edge.history = None;
    }
    for message in &mut before.messages {
        message.history = None;
    }
    before
}

fn dg_entry(spec: DiagramSpec) -> DiagramEntry {
    DiagramEntry {
        plan: PlanEntry {
            key: spec.key.clone(),
            kind: spec.kind,
            title: spec.title.clone(),
            purpose: spec.caption.clone(),
        },
        state: EntryState::Ready {
            spec,
            notes: Vec::new(),
        },
    }
}

/// The review's drawings, oldest first: version 1 at an older commit with the flow, the
/// sequence and the classes, and version 2 at the demo head with the eight diagrams of
/// the demo review (the three of version 1 carry their history).
/// They do not depend on the review; `_pr` names the demo review they belong to.
pub fn demo_diagrams(now: i64, _pr: &PrRef) -> Vec<DiagramVersion> {
    let specs = vec![
        dg_flow(),
        dg_deps(),
        dg_sequence(),
        dg_er(),
        dg_usecase(),
        dg_dataflow(),
        dg_states(),
        dg_classes(),
    ];
    let first = specs
        .iter()
        .filter(|s| ["flow", "sequence", "classes"].contains(&s.key.as_str()))
        .map(|s| dg_entry(dg_before(s)))
        .collect();
    vec![
        DiagramVersion {
            number: 1,
            commit: DIAGRAMS_V1_COMMIT.into(),
            harness: "Claude Code".into(),
            drawn_at: now - 86_400,
            phase: VersionPhase::Done,
            entries: first,
        },
        DiagramVersion {
            number: 2,
            commit: HEAD_SHA.into(),
            harness: "Claude Code".into(),
            drawn_at: now - 3_600,
            phase: VersionPhase::Done,
            entries: specs.into_iter().map(dg_entry).collect(),
        },
    ]
}

/// `demo_diagrams` as the tell a scene sends through the outbox: both versions listed, the
/// second one whole, nothing running, and no new commit.
pub fn demo_diagrams_tell(pr: &PrRef, now: i64) -> Tell {
    let versions = demo_diagrams(now, pr);
    Tell::Diagrams(DiagramsTell::Loaded {
        pr: pr.clone(),
        versions: versions.iter().map(DiagramVersion::info).collect(),
        latest: versions.last().cloned().map(Box::new),
        state: DiagramsState::Idle,
        head: HEAD_SHA.into(),
        stale: false,
        stale_reason: None,
    })
}

/// The mockups' two results (`Security.png`, `Audits.png`): three security findings, and an
/// audit with one finding in Correctness, Concurrency and Tests, the other areas ok. Every
/// finding sits on a new-side line of the demo diff.
pub fn demo_checks(now: i64) -> (Vec<CheckResult>, Vec<CheckStatus>) {
    fn finding(
        kind: CheckKind,
        area: &str,
        severity: Option<Severity>,
        (file, line): (&str, u32),
        (title, body, comment): (&str, &str, &str),
        code: Option<&str>,
    ) -> Finding {
        Finding {
            id: finding_id(area, file, Some(line), title),
            kind,
            area: area.into(),
            severity,
            title: title.into(),
            file: file.into(),
            line: Some(line),
            start_line: None,
            end_line: None,
            body: body.into(),
            comment: comment.into(),
            code: code.map(Into::into),
            anchored: true,
        }
    }
    fn pass(area: &str, text: &str, place: &str) -> Pass {
        Pass {
            area: area.into(),
            text: text.into(),
            place: Some(place.into()),
        }
    }
    let security = |severity, place, text, code| {
        finding(
            CheckKind::Security,
            "security",
            Some(severity),
            place,
            text,
            code,
        )
    };
    let audit = |area, place, text| finding(CheckKind::Audit, area, None, place, text, None);
    let security_result = CheckResult {
        kind: CheckKind::Security,
        head: HEAD_SHA.into(),
        files: 7,
        findings: vec![
            security(
                Severity::High,
                ("src/client/http.rs", 17),
                (
                    "Refresh token written to the debug log",
                    "With RUST_LOG=debug, the client logs this request in full, and its body includes the refresh token. Anyone who can read the logs can sign in as the user.",
                    "This logs the whole request body, refresh token included. Could we log the request without its body?",
                ),
                Some(
                    "-            .execute(request.clone())\n+            .execute(request.redacted_for_logs())",
                ),
            ),
            security(
                Severity::Medium,
                ("src/client/http.rs", 24),
                (
                    "No limit on refresh retries",
                    "A token endpoint that keeps failing makes the client retry forever, which can lock the account by rate limit.",
                    "Could we stop after a few refresh attempts and report the error?",
                ),
                None,
            ),
            security(
                Severity::Low,
                ("src/auth/store.rs", 88),
                (
                    "Token file created with default permissions",
                    "The token file is created with the default umask (usually 0644), so other local users can read it. Create it with 0600.",
                    "Could we create the token file with mode 0600 so other users cannot read it?",
                ),
                None,
            ),
        ],
        passes: Vec::new(),
        unreadable: 0,
        areas: Vec::new(),
        at: now - 120,
    };
    let audit_result = CheckResult {
        kind: CheckKind::Audit,
        head: HEAD_SHA.into(),
        files: 7,
        findings: vec![
            audit(
                "correctness",
                ("src/auth/refresh.rs", 41),
                (
                    "The expiry is checked before the lock is taken",
                    "The token is judged fresh before the refresh lock is held, so two tasks can both decide it is expired and both exchange it.",
                    "Could we check the expiry again after taking the lock, so only one task exchanges the token?",
                ),
            ),
            audit(
                "concurrency",
                ("src/auth/refresh.rs", 44),
                (
                    "The refresh lock is held across a network call",
                    "Every request that needs a token waits on `refresh_lock` while one task talks to GitHub. With a slow endpoint, the whole client stalls for the length of the exchange.",
                    "Could we re-check the expiry after taking the lock, and release it before calling `exchange`? Other tasks would then reuse the fresh token instead of queueing.",
                ),
            ),
            audit(
                "tests",
                ("tests/refresh.rs", 28),
                (
                    "The retry after a revoked token has no test",
                    "The new tests cover the early refresh and two refreshes at once, but not the second attempt after a 401.",
                    "Could we add a test where the first request answers 401 and the retry succeeds?",
                ),
            ),
        ],
        passes: vec![
            pass(
                "concurrency",
                "No shared state written outside the lock",
                "checked 4 call sites",
            ),
            pass(
                "concurrency",
                "The new test covers two tasks refreshing at once",
                "tests/refresh.rs:12",
            ),
            pass(
                "error-handling",
                "Every exchange error reaches the caller",
                "checked 3 call sites",
            ),
            pass(
                "performance",
                "The cached token avoids a disk read per request",
                "src/auth/store.rs:76",
            ),
            pass(
                "docs",
                "The changelog names the early refresh",
                "CHANGELOG.md",
            ),
        ],
        unreadable: 0,
        areas: [
            "correctness",
            "concurrency",
            "error-handling",
            "performance",
            "tests",
            "docs",
        ]
        .map(String::from)
        .to_vec(),
        at: now - 120,
    };
    (
        vec![security_result, audit_result],
        vec![
            CheckStatus {
                kind: CheckKind::Security,
                state: CheckState::Done,
            },
            CheckStatus {
                kind: CheckKind::Audit,
                state: CheckState::Done,
            },
        ],
    )
}

/// The rows of `WhatsNew.png` ("Since you last looked, yesterday …").
/// The draft of the revalidation scenes and the finished round over it: the demo draft grown to
/// five items, and one proposal for each kind of row (kept, moved, obsolete, new wording and
/// not checked). The round only proposes; the draft is exactly what the demo draft says.
pub fn demo_revalidation(now: i64) -> (Draft, Revalidation) {
    let anchor = |path: &str, line: u32| Anchor {
        path: path.into(),
        line,
        start_line: None,
        side: Side::Right,
        commit: HEAD_SHA.into(),
    };
    let mut draft = demo_draft(now);
    for (id, path, line, body) in [
        (
            "i4",
            "tests/refresh.rs",
            18,
            "Missing a test for concurrent refreshes.",
        ),
        (
            "i5",
            "src/auth/store.rs",
            96,
            "Name the temp file after the process id so two reviews never share one.",
        ),
    ] {
        draft.items.push(DraftItem {
            id: id.into(),
            kind: DraftKind::LineComment,
            origin: Origin::Human,
            anchor: Some(anchor(path, line)),
            body: body.into(),
            status: ItemStatus::Ok,
            accepted: true,
            created_at: now - 3000,
            thread: None,
        });
    }
    draft.next_id = 5;
    let mut before: BTreeMap<String, Option<Anchor>> = draft
        .items
        .iter()
        .map(|item| (item.id.clone(), item.anchor.clone()))
        .collect();
    // The deterministic relocation already moved i2 from line 81 before the agent looked.
    before.insert("i2".into(), Some(anchor("src/auth/store.rs", 81)));
    let proposal = |id: &str, verdict: Verdict| {
        let item = draft
            .items
            .iter()
            .find(|item| item.id == id)
            .expect("the demo draft has this item");
        Proposal {
            item: id.into(),
            before: before[id].clone(),
            verdict,
            decision: Decision::Pending,
            fingerprint: fingerprint(item),
        }
    };
    let proposals = vec![
        proposal("i1", Verdict::Kept),
        proposal(
            "i2",
            Verdict::Moved {
                anchor: anchor("src/auth/store.rs", 88),
                reason: "save() moved down 7 lines when the temp-file helper was added; the comment follows the same code.".into(),
            },
        ),
        proposal(
            "i3",
            Verdict::Obsolete {
                reason: "The retry loop was removed in \"refactor: one retry path\".".into(),
            },
        ),
        proposal(
            "i4",
            Verdict::Reword {
                text: "The new test covers two refreshes at once; a refresh racing invalidate() is still missing.".into(),
                reason: None,
            },
        ),
        proposal("i5", Verdict::NotChecked),
    ];
    let round = Revalidation {
        from_head: "7d3c5a1e9b2f4a6c8e0d1f3b5a7c9e2d4f6a8b0c".into(),
        to_head: HEAD_SHA.into(),
        commits: vec![
            CommitLine {
                sha: "c41d2e7".into(),
                title: "refactor: one retry path".into(),
            },
            CommitLine {
                sha: "a98f03b".into(),
                title: "fix: write the token through a temp file".into(),
            },
        ],
        before,
        sp1_moved: 1,
        sp1_obsolete: 1,
        state: RoundState::Done,
        proposals,
        unreadable: 0,
        asked: 5,
        finished_at: Some(now - 120),
        notified: false,
        diagrams_stale: None,
    };
    (draft, round)
}

fn demo_news(now: i64) -> Vec<NewsItem> {
    let item = |kind, source: &str, who: Option<&str>, age: i64, summary: &str| NewsItem {
        kind,
        source: source.into(),
        who: who.map(String::from),
        at: now - age,
        summary: summary.into(),
        url: None,
    };
    vec![
        item(
            NewsKind::Commits,
            "GitHub",
            Some("octo"),
            8 * 3600,
            "2 new commits: fix: re-check the expiry under the lock · test: refresh races",
        ),
        item(
            NewsKind::Comment,
            "GitHub",
            Some("mona, hubot"),
            6 * 3600,
            "3 new comments: Why one minute? The CLI uses 30 seconds.",
        ),
        item(
            NewsKind::Checks,
            "GitHub Actions",
            None,
            5 * 3600,
            "Checks passed: build · clippy · test (macOS)",
        ),
        item(
            NewsKind::Moved,
            "Clúsia",
            None,
            4 * 3600,
            "Your draft followed the code: 1 moved, 1 obsolete — store.rs:81 → 88 · http.rs:20 \
             no longer in the diff",
        ),
        item(
            NewsKind::Local,
            "You via CLI",
            None,
            3600,
            "Note added: Ask about the token cache size",
        ),
    ]
}

/// One file of the demo diff; the counts come from the patch.
fn file(path: &str, status: &str, lines: &[&str]) -> FileDiff {
    let patch = format!("{}\n", lines.join("\n"));
    let count = |marker: char| {
        lines
            .iter()
            .filter(|l| l.starts_with(marker) && !l.starts_with("@@"))
            .count() as u64
    };
    FileDiff {
        path: path.into(),
        previous_path: None,
        status: status.into(),
        additions: count('+'),
        deletions: count('-'),
        patch: Some(patch),
    }
}

fn demo_diff() -> Vec<FileDiff> {
    vec![
        file(
            "src/auth/refresh.rs",
            "modified",
            &[
                "@@ -38,9 +38,11 @@ impl TokenStore {",
                "     pub async fn refresh(&self) -> Result<Token, AuthError> {",
                "-        let token = self.load()?;",
                "-        if token.expires_at > now() {",
                "+        let token = self.load().await?;",
                "+        // Refresh one minute early so a request never races the expiry.",
                "+        if token.expires_at > now() + Duration::from_secs(60) {",
                "             return Ok(token);",
                "         }",
                "+        let _guard = self.refresh_lock.lock().await;",
                "         let fresh = self.client.exchange(&token.refresh).await?;",
                "         self.save(&fresh)?;",
                "         Ok(fresh)",
                "     }",
                "@@ -60,4 +62,9 @@ impl TokenStore {",
                "     fn expired(&self, token: &Token) -> bool {",
                "         token.expires_at <= now()",
                "     }",
                "+",
                "+    /// Drops the cached token so the next call refreshes it.",
                "+    pub fn invalidate(&self) {",
                "+        self.cache.lock().take();",
                "+    }",
                " }",
            ],
        ),
        file(
            "src/auth/store.rs",
            "modified",
            &[
                "@@ -70,22 +70,51 @@ impl TokenStore {",
                "     pub fn new(path: PathBuf) -> Self {",
                "         Self { path, cache: Mutex::new(None) }",
                "     }",
                " ",
                "-    pub fn load(&self) -> Result<Token, AuthError> {",
                "-        let data = fs::read(&self.path).map_err(AuthError::Read)?;",
                "-        serde_json::from_slice(&data).map_err(AuthError::Decode)",
                "-    }",
                "-",
                "-    pub fn save(&self, token: &Token) -> Result<(), AuthError> {",
                "-        let data = serde_json::to_vec(token).map_err(AuthError::Encode)?;",
                "-        let mut file = File::create(&self.path).map_err(AuthError::Write)?;",
                "-        file.write_all(&data).map_err(AuthError::Write)?;",
                "-        Ok(())",
                "-    }",
                "-",
                "+    /// Reads the token, from memory when it is still cached.",
                "+    pub fn load(&self) -> Result<Token, AuthError> {",
                "+        if let Some(token) = self.cache.lock().clone() {",
                "+            return Ok(token);",
                "+        }",
                "+        let data = fs::read(&self.path).map_err(AuthError::Read)?;",
                "+        let token: Token = serde_json::from_slice(&data).map_err(AuthError::Decode)?;",
                "+        *self.cache.lock() = Some(token.clone());",
                "+        Ok(token)",
                "+    }",
                "+",
                "+    /// Writes the token and updates the cache.",
                "+    pub fn save(&self, token: &Token) -> Result<(), AuthError> {",
                "+        let data = serde_json::to_vec_pretty(token).map_err(AuthError::Encode)?;",
                "+        fs::write(&self.path, data).map_err(AuthError::Write)?;",
                "+        *self.cache.lock() = Some(token.clone());",
                "+        Ok(())",
                "+    }",
                "+",
                "+    /// Removes the stored token and forgets the cached one.",
                "+    pub fn clear(&self) -> Result<(), AuthError> {",
                "+        match fs::remove_file(&self.path) {",
                "+            Ok(()) => {}",
                "+            Err(e) if e.kind() == io::ErrorKind::NotFound => {}",
                "+            Err(e) => return Err(AuthError::Write(e)),",
                "+        }",
                "+        self.cache.lock().take();",
                "+        Ok(())",
                "+    }",
                "+",
                "+    /// Whether a token file exists, without reading it.",
                "+    pub fn exists(&self) -> bool {",
                "+        self.path.is_file()",
                "+    }",
                "+",
                "+    /// The token's expiry, if one is stored.",
                "+    pub fn expires_at(&self) -> Option<Instant> {",
                "+        self.load().ok().map(|t| t.expires_at)",
                "+    }",
                "+",
                "+    // Callers hold the refresh lock while they save.",
                "     pub fn path(&self) -> &Path {",
                "         &self.path",
                "     }",
                " }",
                " ",
                " #[cfg(test)]",
            ],
        ),
        file(
            "src/auth/mod.rs",
            "modified",
            &[
                "@@ -1,6 +1,8 @@",
                " //! Authentication: tokens, their storage and refresh.",
                " ",
                "-pub mod store;",
                "+pub(crate) mod store;",
                "+mod lock;",
                " pub mod refresh;",
                "+",
                " pub use refresh::Refresher;",
                " pub use store::TokenStore;",
            ],
        ),
        file(
            "src/client/http.rs",
            "modified",
            &[
                "@@ -12,15 +12,34 @@ impl HttpClient {",
                "     pub async fn send(&self, request: Request) -> Result<Response, HttpError> {",
                "         let token = self.auth.refresh().await?;",
                "         let request = request.bearer(&token.access);",
                "-        let mut attempt = 0;",
                "-        loop {",
                "-            match self.inner.execute(request.clone()).await {",
                "-                Ok(response) => return Ok(response),",
                "-                Err(e) if attempt < 3 => attempt += 1,",
                "-                Err(e) => return Err(HttpError::Send(e)),",
                "-            }",
                "-            sleep(Duration::from_millis(200 * attempt)).await;",
                "-        }",
                "+        let response = self",
                "+            .inner",
                "+            .execute(request.clone())",
                "+            .await",
                "+            .map_err(HttpError::Send)?;",
                "+        if response.status() != StatusCode::UNAUTHORIZED {",
                "+            return Ok(response);",
                "+        }",
                "+        // The token was revoked early: refresh once and retry.",
                "+        self.auth.invalidate();",
                "+        let token = self.auth.refresh().await?;",
                "+        let retry = request.bearer(&token.access);",
                "+        let response = self",
                "+            .inner",
                "+            .execute(retry)",
                "+            .await",
                "+            .map_err(HttpError::Send)?;",
                "+        if response.status() == StatusCode::UNAUTHORIZED {",
                "+            return Err(HttpError::Unauthorized);",
                "+        }",
                "+        Ok(response)",
                "     }",
                " ",
                "+    /// The base URL every request is relative to.",
                "+    #[must_use]",
                "+    pub fn base(&self) -> &Url {",
                "+        &self.base",
                "+    }",
                "+",
                "+    /// Headers sent with every request.",
                "     fn headers(&self) -> HeaderMap {",
            ],
        ),
        file(
            "src/config.rs",
            "modified",
            &[
                "@@ -20,4 +20,10 @@ impl Default for Config {",
                " pub struct Config {",
                "     pub api: Url,",
                "     pub timeout: Duration,",
                "+    /// Refresh this long before the token expires.",
                "+    #[serde(default = \"default_refresh_margin\")]",
                "+    pub refresh_margin: Duration,",
                "+    /// Where the token is stored.",
                "+    pub token_path: PathBuf,",
                "+    pub user_agent: String,",
                " }",
            ],
        ),
        file(
            "tests/refresh.rs",
            "added",
            &[
                "@@ -0,0 +1,30 @@",
                "+use std::time::Duration;",
                "+",
                "+use auth::{Token, TokenStore};",
                "+",
                "+mod common;",
                "+",
                "+#[tokio::test]",
                "+async fn refreshes_one_minute_early() {",
                "+    let server = common::token_server().await;",
                "+    let store = TokenStore::new(server.dir().join(\"token.json\"));",
                "+    store.save(&Token::expiring_in(Duration::from_secs(30))).unwrap();",
                "+    let token = server.client(&store).refresh().await.unwrap();",
                "+    assert_eq!(server.exchanges(), 1);",
                "+    assert!(token.expires_at > Token::now() + Duration::from_secs(60));",
                "+}",
                "+",
                "+#[tokio::test]",
                "+async fn concurrent_refreshes_exchange_once() {",
                "+    let server = common::token_server().await;",
                "+    let store = TokenStore::new(server.dir().join(\"token.json\"));",
                "+    store.save(&Token::expired()).unwrap();",
                "+    let client = server.client(&store);",
                "+    let (a, b) = tokio::join!(client.refresh(), client.refresh());",
                "+    assert_eq!(a.unwrap(), b.unwrap());",
                "+    assert_eq!(server.exchanges(), 1);",
                "+}",
                "+",
                "+// A revoked token is refreshed once by the HTTP client, then the request",
                "+// is sent again; see `client::http` for the retry.",
                "+// Clock skew on CI runners is why the margin is one minute.",
            ],
        ),
        file(
            "CHANGELOG.md",
            "modified",
            &[
                "@@ -1,14 +1,7 @@",
                " # Changelog",
                " ",
                " ## Unreleased",
                "-",
                "-## 0.4.0",
                "-",
                "-- Token store keeps the token in memory.",
                "-- `clusia auth status` shows the token source.",
                "-",
                "-## 0.3.0",
                "-",
                "-- First release with GitHub sign-in.",
                "-- Pull request lists in the tray.",
                "+",
                "+- Tokens refresh one minute before they expire, under a lock.",
                "+- Older entries moved to `docs/HISTORY.md`.",
                " ",
            ],
        ),
    ]
}

fn rfc3339(unix: i64) -> String {
    let (y, m, d) = civil_from_days(unix.div_euclid(86_400));
    let s = unix.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        s / 3600,
        s / 60 % 60,
        s % 60
    )
}

/// 182 days ending today: busy weekdays, quiet weekends.
fn heat(now: i64) -> Vec<DayCount> {
    let today = now.div_euclid(86_400);
    (0..182)
        .map(|i| {
            let day = today - 181 + i;
            let (y, m, d) = civil_from_days(day);
            let weekday = (day + 4).rem_euclid(7); // 1970-01-01 was a Thursday; 0 = Sunday
            let count = if weekday == 0 || weekday == 6 {
                0
            } else {
                (day * 7919).rem_euclid(5) as u32
            };
            DayCount {
                date: format!("{y:04}-{m:02}-{d:02}"),
                count,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::NOW;
    use clusia_core::checks::anchor_in_diff;

    #[test]
    fn the_codex_demo_is_the_demo_with_codex_chosen() {
        use clusia_core::config::HarnessKind as Kind;
        let snap = demo_codex(1_700_000_000);
        assert_eq!(snap.config.harness.kind, Kind::Codex);
        assert_eq!(
            snap.config.harness.program.as_deref(),
            Some("/opt/homebrew/bin/codex")
        );
        assert_eq!(
            demo(1_700_000_000).config.harness.kind,
            Kind::ClaudeCode,
            "the plain demo stays on Claude Code"
        );
    }

    #[test]
    fn the_codex_probe_names_codex_and_its_login() {
        let p = demo_codex_probe();
        assert!(p.ok);
        assert_eq!(p.harness, "Codex");
        assert_eq!(p.version.as_deref(), Some("0.162.1"));
        assert_eq!(p.account.as_deref(), Some("Logged in using ChatGPT"));
        assert_eq!(p.signed_in, Some(true));
    }

    #[test]
    fn the_custom_demo_is_the_demo_with_a_custom_command_chosen() {
        use clusia_core::config::HarnessKind as Kind;
        let snap = demo_custom(1_700_000_000);
        assert_eq!(snap.config.harness.kind, Kind::Custom);
        assert_eq!(
            snap.config.harness.program.as_deref(),
            Some("/usr/local/bin/echo-harness")
        );
        assert_eq!(snap.config.harness.custom_name, "Echo");
        assert_eq!(snap.config.harness.display_name(), "Echo");
        assert_eq!(
            demo(1_700_000_000).config.harness.kind,
            Kind::ClaudeCode,
            "the plain demo stays on Claude Code"
        );
    }

    #[test]
    fn the_custom_probe_names_echo_and_has_no_account() {
        let p = demo_custom_probe();
        assert!(p.ok);
        assert_eq!(p.harness, "Echo");
        assert_eq!(p.version.as_deref(), Some("1.0"));
        assert_eq!(p.elapsed_ms, 200);
        assert_eq!((p.signed_in, p.account), (None, None));
    }

    #[test]
    fn the_other_request_has_no_rule_to_grant() {
        let r = demo_permission_other(1_000);
        assert_eq!(r.tool, "Other");
        assert_eq!(r.summary, "Remove the build cache");
        assert_eq!(r.prefix, None);
        assert!(r.reason.is_some());
        assert_eq!(r.deadline, 113_000);
    }

    #[test]
    fn first_run_can_find_both_harnesses() {
        let f = demo_first_run_both();
        assert!(f.harnesses.iter().all(|h| h.path.is_some()));
        assert_eq!(f.folders, demo_first_run().folders);
    }

    #[test]
    fn the_demo_permission_request_is_the_mockups() {
        let r = demo_permission_request(1_790_000_000_000);
        assert_eq!(r.pr, demo_pr());
        assert_eq!(r.tool, "Bash");
        assert_eq!(
            r.summary,
            "cargo test -p clusia-auth refresh_race -- --nocapture"
        );
        assert_eq!(
            r.reason.as_deref(),
            Some("To check that two refreshes at once don't exchange the token twice.")
        );
        assert_eq!(r.prefix.as_deref(), Some("cargo test"));
        assert!(r.sandbox);
        assert_eq!(r.deadline, 1_790_000_112_000, "1:52 left when it is asked");
        assert_eq!(r.detail, None, "a command has no diff");
        assert_eq!(demo_permission_rules(), ["Bash(cargo test:*)"]);
    }

    #[test]
    fn the_demo_agent_log_has_the_mockup_chat() {
        use clusia_protocol::{AgentLogEntry, PermissionOutcome};
        let turns = demo_agent_turns(1_790_000_000);
        assert_eq!(turns.len(), 2);
        let all = demo_agent_log(1_790_000_000);
        assert_eq!(all.len(), turns.iter().map(Vec::len).sum::<usize>());
        let (first, second) = (&turns[0], &turns[1]);
        assert!(first.iter().any(|e| matches!(
            e,
            AgentLogEntry::Permission { tool, summary, outcome: PermissionOutcome::Denied, .. }
                if tool == "Bash" && summary == "cargo test"
        )));
        assert!(
            first
                .iter()
                .any(|e| matches!(e, AgentLogEntry::Text { text, .. } if text.contains("Summary")))
        );
        assert!(matches!(
            &second[0],
            AgentLogEntry::User { text, .. } if text == "Is the new lock needed at all, or would re-checking the expiry be enough?"
        ));
        let tools: Vec<&str> = second
            .iter()
            .filter_map(|e| match e {
                AgentLogEntry::ToolUse { summary, .. } => Some(summary.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            tools,
            [
                "Read src/auth/store.rs and client/http.rs",
                "Searched for refresh_lock · 3 uses"
            ]
        );
        let suggestion = demo_suggestion();
        assert!(second.iter().any(
            |e| matches!(e, AgentLogEntry::Suggestion { suggestion: s, .. } if *s == suggestion)
        ));
        assert_eq!(demo_suggestions(), std::slice::from_ref(&suggestion));
        assert_eq!(suggestion.file, "src/auth/refresh.rs");
        assert_eq!(suggestion.line, Some(44));
        assert!(suggestion.id.starts_with("sug-") && suggestion.id.len() == 16);
        // Every turn ends the way the daemon's log does.
        for turn in &turns {
            assert!(matches!(turn.last(), Some(AgentLogEntry::Done { .. })));
        }
    }

    #[test]
    fn the_demo_suggestion_is_on_a_commentable_line() {
        let patch = demo_diff()
            .into_iter()
            .find(|f| f.path == demo_suggestion().file)
            .and_then(|f| f.patch)
            .expect("the file is in the demo diff");
        assert!(clusia_core::can_comment(&patch, Side::Right, 44));
    }

    #[test]
    fn the_demo_probe_is_a_good_answer() {
        let p = demo_probe();
        assert!(p.ok && p.error.is_none());
        assert_eq!(p.version.as_deref(), Some("2.1.294"));
        assert_eq!(p.elapsed_ms, 1800);
    }

    #[test]
    fn demo_uses_only_generic_data() {
        let s = demo(1_790_000_000);
        let owners = s
            .assigned
            .iter()
            .chain(&s.mine)
            .map(|p| &p.pr)
            .chain(s.reviews.iter().map(|r| &r.pr));
        for pr in owners {
            assert_eq!(pr.owner, "rzorzal");
        }
        for p in s.assigned.iter().chain(&s.mine) {
            assert!(["octo", "mona", "hubot", "rzorzal"].contains(&p.author.as_str()));
        }
        assert_eq!(s.activity.as_ref().unwrap().heatmap.len(), 182);
        assert!(s.lists_loaded);
        assert_eq!(s.config, Config::default());
    }

    #[test]
    fn demo_review_matches_the_mockup() {
        let (view, news) = demo_review(NOW);
        assert_eq!(view.review.pr, demo_pr());
        assert_eq!(view.pr.summary.title, "feat: auth refresh");
        assert_eq!(view.pr.summary.author, "octo");
        assert_eq!(
            (view.pr.head_ref.as_str(), view.pr.base_ref.as_str()),
            ("octo:auth-refresh", "main")
        );
        assert_eq!(
            (view.pr.additions, view.pr.deletions, view.pr.changed_files),
            (120, 34, 7)
        );
        let stats: Vec<(&str, u64, u64)> = view
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.additions, f.deletions))
            .collect();
        assert_eq!(
            stats,
            [
                ("src/auth/refresh.rs", 9, 2),
                ("src/auth/store.rs", 41, 12),
                ("src/auth/mod.rs", 3, 1),
                ("src/client/http.rs", 28, 9),
                ("src/config.rs", 6, 0),
                ("tests/refresh.rs", 30, 0),
                ("CHANGELOG.md", 3, 10),
            ]
        );
        assert_eq!(view.checks.unwrap().label(), "passed");
        assert_eq!(view.role, Role::Reviewer);
        let conversation = view.conversation.as_ref().unwrap();
        let open: Vec<&ReviewThread> = conversation
            .review_threads
            .iter()
            .filter(|t| !t.is_resolved)
            .collect();
        assert_eq!(open.len(), 1);
        assert_eq!(
            (open[0].path.as_str(), open[0].line),
            ("src/auth/refresh.rs", Some(41))
        );
        assert_eq!(open[0].comments[0].author, "mona");
        assert_eq!(
            open[0].comments[0].body,
            "Why one minute? The CLI uses 30 seconds."
        );
        assert_eq!(
            conversation.review_threads.len()
                + conversation.comments.len()
                + conversation
                    .reviews
                    .iter()
                    .filter(|r| !r.body.is_empty())
                    .count(),
            5,
            "the Comments tab count of the mockup"
        );
        assert_eq!(
            conversation.reviews[0].body,
            "Looks good once the lock change lands."
        );
        let items: Vec<(String, u32, &ItemStatus)> = view
            .review
            .draft
            .items
            .iter()
            .map(|i| {
                let a = i.anchor.as_ref().unwrap();
                (a.path.clone(), a.line, &i.status)
            })
            .collect();
        assert_eq!(items.len(), 3);
        assert_eq!(
            (items[0].0.as_str(), items[0].1),
            ("src/auth/refresh.rs", 44)
        );
        assert!(matches!(
            items[1].2,
            ItemStatus::Moved { from_line: 81, .. }
        ));
        assert!(matches!(items[2].2, ItemStatus::Obsolete { .. }));
        assert_eq!(news.len(), 5);
        assert!(
            news.iter()
                .all(|n| n.at > view.review.last_seen_at.unwrap())
        );
    }

    #[test]
    fn demo_patches_agree_with_their_counts_and_anchors() {
        use clusia_core::can_comment;
        use clusia_view::diff::{RowKind, parse_patch};

        let (view, _) = demo_review(NOW);
        for f in &view.diff {
            let rows = parse_patch(f.patch.as_deref().unwrap());
            let count = |kind| rows.iter().filter(|r| r.kind == kind).count() as u64;
            assert_eq!(
                (count(RowKind::Added), count(RowKind::Removed)),
                (f.additions, f.deletions),
                "{}",
                f.path
            );
        }
        let patch = |path: &str| {
            view.diff
                .iter()
                .find(|f| f.path == path)
                .and_then(|f| f.patch.clone())
                .unwrap()
        };
        assert!(can_comment(&patch("src/auth/refresh.rs"), Side::Right, 44));
        assert!(can_comment(&patch("src/auth/refresh.rs"), Side::Right, 41));
        assert!(can_comment(&patch("src/auth/store.rs"), Side::Right, 88));
        assert!(can_comment(&patch("src/auth/mod.rs"), Side::Right, 3));
    }

    #[test]
    fn demo_gif_is_a_real_animated_gif() {
        use image::AnimationDecoder;
        let bytes = demo_gif_bytes("https://media.giphy.com/media/demo-party/giphy.gif");
        assert!(bytes.starts_with(b"GIF8"));
        let decoder = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes)).unwrap();
        let frames = decoder.into_frames().collect_frames().unwrap();
        assert_eq!(frames.len(), 6);
        assert_eq!(frames[0].buffer().dimensions(), (120, 90));
        assert_ne!(
            demo_gif_bytes("https://media.giphy.com/media/demo-thumbs/giphy.gif"),
            demo_gif_bytes("https://media.giphy.com/media/demo-party/giphy.gif"),
            "each GIF has its own color"
        );
    }

    #[test]
    fn demo_media_only_serves_giphy() {
        let (kind, bytes) =
            demo_media("https://media.giphy.com/media/demo-clap/giphy.gif").unwrap();
        assert_eq!(kind, MediaKind::Gif);
        assert!(!bytes.is_empty());
        assert!(demo_media("https://example.org/pic.png").is_none());
        assert!(demo_media("http://media.giphy.com/media/a/giphy.gif").is_none());
    }

    #[test]
    fn demo_gif_search_filters_by_title() {
        let titles = |q: &str, offset| -> Vec<String> {
            demo_gif_page(q, offset)
                .items
                .into_iter()
                .map(|i| i.title)
                .collect()
        };
        assert_eq!(titles("", 0).len(), 6, "trending");
        assert_eq!(titles(" TURTLE ", 0), ["party turtle"]);
        assert!(titles("nothing like it", 0).is_empty());
        assert!(titles("", 24).is_empty(), "only the first page has results");
        let item = &demo_gif_page("ship", 0).items[0];
        assert_eq!(
            item.preview_url,
            "https://media.giphy.com/media/demo-ship/100w.gif"
        );
        assert_eq!(
            item.url,
            "https://media.giphy.com/media/demo-ship/giphy.gif"
        );
    }

    #[test]
    fn demo_comments_show_rich_text() {
        let comments = demo_review(NOW).0.conversation.unwrap().comments;
        let bodies: Vec<&str> = comments.iter().map(|c| c.body.as_str()).collect();
        assert!(bodies[0].contains("**refresh works**") && bodies[0].contains("```rust"));
        assert!(bodies[0].contains(":white_check_mark:"));
        assert!(bodies[1].contains(":tada:") && bodies[1].contains("🐢"));
        assert!(bodies[1].contains("![party](https://media.giphy.com/media/demo-party/giphy.gif)"));
        assert!(bodies[1].contains("[auth guide](https://github.com/rzorzal/clusia/pull/98)"));
    }

    #[test]
    fn demo_first_run_matches_the_mockup() {
        let f = demo_first_run();
        assert!(matches!(&f.github, GithubLogin::SignedIn { login, .. } if login == "rzorzal"));
        let chips: Vec<(&str, bool, u32)> = f
            .folders
            .iter()
            .map(|d| (d.path.as_str(), d.exists, d.repos))
            .collect();
        assert_eq!(
            chips,
            [
                ("~/Repos", true, 12),
                ("~/Projects", true, 3),
                ("~/src", true, 0),
                ("~/code", false, 0)
            ]
        );
        assert_eq!(f.harnesses.len(), 2, "the daemon reports both kinds");
        assert_eq!(f.harnesses[0].kind, HarnessKind::ClaudeCode);
        assert!(f.harnesses[0].path.is_some() && f.harnesses[1].path.is_none());
    }

    #[test]
    fn demo_checks_match_the_mockups() {
        let (results, states) = demo_checks(NOW);
        let security = results
            .iter()
            .find(|r| r.kind == CheckKind::Security)
            .expect("a security result");
        let titles: Vec<&str> = security.findings.iter().map(|f| f.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "Refresh token written to the debug log",
                "No limit on refresh retries",
                "Token file created with default permissions",
            ]
        );
        assert_eq!(security.files, 7);
        let audit = results
            .iter()
            .find(|r| r.kind == CheckKind::Audit)
            .expect("an audit result");
        assert_eq!(
            audit.areas,
            [
                "correctness",
                "concurrency",
                "error-handling",
                "performance",
                "tests",
                "docs"
            ]
        );
        let per_area = |area: &str| audit.findings.iter().filter(|f| f.area == area).count();
        assert_eq!(
            [
                per_area("correctness"),
                per_area("concurrency"),
                per_area("error-handling"),
                per_area("performance"),
                per_area("tests"),
                per_area("docs"),
            ],
            [1, 1, 0, 0, 1, 0]
        );
        assert_eq!(
            audit
                .passes
                .iter()
                .filter(|p| p.area == "concurrency")
                .count(),
            2
        );
        assert_eq!(states.len(), 2);
        assert!(matches!(
            demo_checks_tell(&demo_pr(), NOW, &demo_review(NOW).0.review.draft),
            Tell::Checks(ChecksTell::Loaded { .. })
        ));
    }

    #[test]
    fn the_demo_drawings_are_two_versions_of_the_mockup_diagrams() {
        let versions = demo_diagrams(NOW, &demo_pr());
        assert_eq!(
            versions.iter().map(|v| v.number).collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(versions[1].commit, HEAD_SHA);
        assert_ne!(versions[0].commit, HEAD_SHA);
        let keys = |v: &DiagramVersion| -> Vec<String> {
            v.entries.iter().map(|e| e.plan.key.clone()).collect()
        };
        assert_eq!(
            keys(&versions[1]),
            [
                "flow", "deps", "sequence", "er", "usecase", "dataflow", "states", "classes"
            ]
        );
        assert_eq!(keys(&versions[0]), ["flow", "sequence", "classes"]);
        let kinds: Vec<DiagramKind> = versions[1].entries.iter().map(|e| e.plan.kind).collect();
        assert_eq!(kinds, DiagramKind::ALL);
        assert!(versions.iter().all(|v| v.harness == "Claude Code"));
    }

    #[test]
    fn the_demo_drawings_hold_together() {
        let files: Vec<String> = demo_diff().into_iter().map(|f| f.path).collect();
        for version in demo_diagrams(NOW, &demo_pr()) {
            for entry in &version.entries {
                let EntryState::Ready { spec, .. } = &entry.state else {
                    panic!("every demo entry is ready");
                };
                let ids: Vec<&str> = spec.nodes.iter().map(|n| n.id.as_str()).collect();
                let mut unique = ids.clone();
                unique.sort_unstable();
                unique.dedup();
                assert_eq!(unique.len(), ids.len(), "{}: unique node ids", spec.key);
                for e in &spec.edges {
                    assert!(
                        ids.contains(&e.from.as_str()) && ids.contains(&e.to.as_str()),
                        "{}: edge {}>{}",
                        spec.key,
                        e.from,
                        e.to
                    );
                }
                for m in &spec.messages {
                    assert!(ids.contains(&m.from.as_str()) && ids.contains(&m.to.as_str()));
                }
                for g in &spec.groups {
                    assert!(g.members.iter().all(|m| ids.contains(&m.as_str())));
                }
                for a in spec.nodes.iter().filter_map(|n| n.anchor.as_ref()) {
                    assert!(
                        files.contains(&a.path),
                        "{} is a file of the demo diff",
                        a.path
                    );
                    assert!(0 < a.start_line && a.start_line <= a.end_line);
                }
                assert_eq!(
                    spec.messages.is_empty(),
                    spec.kind != DiagramKind::Sequence,
                    "{}: only a sequence has messages",
                    spec.key
                );
            }
        }
    }

    #[test]
    fn version_two_marks_what_changed_against_version_one() {
        let versions = demo_diagrams(NOW, &demo_pr());
        let spec = |v: usize, key: &str| match &versions[v].entry(key).unwrap().state {
            EntryState::Ready { spec, .. } => spec.clone(),
            _ => panic!("ready"),
        };
        let flow = spec(1, "flow");
        let history = |id: &str| flow.nodes.iter().find(|n| n.id == id).unwrap().history;
        assert_eq!(history("lock"), Some(History::Added));
        assert_eq!(history("refresh"), Some(History::Changed));
        assert_eq!(history("http"), Some(History::Same));
        assert!(
            flow.edges
                .iter()
                .any(|e| e.history == Some(History::Removed)),
            "a removed arrow stays in the spec"
        );
        let before = spec(0, "flow");
        assert!(
            before
                .nodes
                .iter()
                .all(|n| n.id != "lock" && n.history.is_none())
        );
        assert_eq!(before.nodes.len(), flow.nodes.len() - 1);
        assert_eq!(spec(1, "sequence").messages.len(), 8);
        assert_eq!(spec(0, "sequence").messages.len(), 6);
        let deps = spec(1, "deps");
        assert!(
            deps.nodes.iter().all(|n| n.history.is_none()),
            "new in version 2"
        );
    }

    #[test]
    fn the_diagrams_tell_lists_both_versions_and_holds_the_latest() {
        let Tell::Diagrams(DiagramsTell::Loaded {
            versions,
            latest,
            state,
            head,
            stale,
            ..
        }) = demo_diagrams_tell(&demo_pr(), NOW)
        else {
            panic!("a loaded tell");
        };
        assert_eq!(versions.len(), 2);
        assert_eq!(latest.map(|v| v.number), Some(2));
        assert_eq!(
            (state, head.as_str(), stale),
            (DiagramsState::Idle, HEAD_SHA, false)
        );
    }

    #[test]
    fn the_draft_marks_the_findings_it_holds_as_accepted() {
        let (view, _) = demo_review(NOW);
        let (results, _) = demo_checks(NOW);
        let medium = results[0].findings[1].clone();
        let mut draft = view.review.draft.clone();
        assert!(demo_accepted(&results, &draft).is_empty());
        draft
            .add_as(
                Origin::Security,
                DraftKind::LineComment,
                Some(Anchor {
                    commit: HEAD_SHA.into(),
                    path: medium.file.clone(),
                    line: medium.line.unwrap(),
                    start_line: None,
                    side: Side::Right,
                }),
                None,
                &medium.comment,
                NOW,
            )
            .unwrap();
        assert_eq!(demo_accepted(&results, &draft), [medium.id.as_str()]);
        let Tell::Checks(ChecksTell::Loaded { accepted, .. }) =
            demo_checks_tell(&demo_pr(), NOW, &draft)
        else {
            panic!("a loaded tell");
        };
        assert_eq!(accepted, [medium.id]);
    }

    #[test]
    fn demo_findings_sit_on_lines_of_the_demo_diff() {
        let (view, _) = demo_review(NOW);
        let (results, _) = demo_checks(NOW);
        for f in results.iter().flat_map(|r| &r.findings) {
            let line = f.line.expect("demo findings name one line");
            assert!(f.anchored, "{} is anchored", f.title);
            assert!(
                anchor_in_diff(&view.diff, &f.file, line, line),
                "{} points into the diff",
                f.title
            );
            assert_eq!(
                f.id,
                clusia_core::checks::finding_id(&f.area, &f.file, Some(line), &f.title)
            );
        }
    }

    #[test]
    fn the_demo_round_has_one_proposal_per_row_kind() {
        use clusia_core::revalidation::{Decision, RoundState, RowKind, pending, row_kind};
        let (draft, round) = demo_revalidation(NOW);
        let ids: Vec<&str> = draft.items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["i1", "i2", "i3", "i4", "i5"]);
        assert_eq!(draft.next_id, 5);
        assert_eq!(round.state, RoundState::Done);
        assert_eq!((round.asked, round.unreadable), (5, 0));
        assert_eq!((round.sp1_moved, round.sp1_obsolete), (1, 1));
        assert_eq!(round.commits.len(), 2);
        assert_eq!(round.to_head, HEAD_SHA);
        assert_ne!(round.from_head, round.to_head);
        let kinds: Vec<RowKind> = round
            .proposals
            .iter()
            .map(|p| {
                let item = draft.items.iter().find(|i| i.id == p.item).unwrap();
                row_kind(p, item)
            })
            .collect();
        assert_eq!(
            kinds,
            [
                RowKind::Kept,
                RowKind::Moved,
                RowKind::Obsolete,
                RowKind::NewWording,
                RowKind::NotChecked,
            ]
        );
        assert!(
            round
                .proposals
                .iter()
                .all(|p| p.decision == Decision::Pending)
        );
        assert_eq!(pending(&round, &draft), 3);
        for p in &round.proposals {
            let item = draft.items.iter().find(|i| i.id == p.item).unwrap();
            assert_eq!(p.fingerprint, clusia_core::revalidation::fingerprint(item));
            assert_eq!(p.before, round.before[&p.item], "{}", p.item);
        }
    }

    #[test]
    fn the_demo_round_changes_nothing_in_the_draft_and_sits_on_the_diff() {
        let (view, _) = demo_review(NOW);
        let (draft, round) = demo_revalidation(NOW);
        assert_eq!(
            draft.items[..3],
            view.review.draft.items[..],
            "the first three items are the demo draft's"
        );
        for item in &draft.items {
            if let (Some(a), true) = (&item.anchor, item.status == ItemStatus::Ok) {
                assert!(
                    anchor_in_diff(&view.diff, &a.path, a.line, a.line),
                    "{} points into the diff",
                    item.id
                );
            }
        }
        // Put it back needs the old line to be commentable at the new head.
        let back = round.before["i2"].as_ref().expect("i2 had an anchor");
        assert_eq!((back.path.as_str(), back.line), ("src/auth/store.rs", 81));
        assert!(anchor_in_diff(&view.diff, &back.path, back.line, back.line));
        let moved = round
            .proposals
            .iter()
            .find(|p| p.item == "i2")
            .expect("i2 has a proposal");
        let clusia_core::revalidation::Verdict::Moved { anchor, reason } = &moved.verdict else {
            panic!("i2 is the moved row")
        };
        assert_eq!(
            (anchor.path.as_str(), anchor.line),
            ("src/auth/store.rs", 88)
        );
        assert!(anchor_in_diff(
            &view.diff,
            &anchor.path,
            anchor.line,
            anchor.line
        ));
        assert!(!reason.is_empty());
    }
}
