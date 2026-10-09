//! Demo data for `--demo` and screenshots: only `rzorzal` repositories and generic people.

use clusia_core::media::MediaKind;
use clusia_core::time::civil_from_days;
use clusia_core::{
    ActivitySummary, Anchor, ChecksSummary, Config, DayCount, Draft, DraftItem, DraftKind,
    FileDiff, IssueComment, ItemStatus, Origin, PrConversation, PrDetail, PrRef, PrSummary, Review,
    ReviewInfo, ReviewState, ReviewThread, Role, Side, ThreadPost,
};
use clusia_protocol::{
    AgentLogEntry, AuthInfo, FileSummary, FirstRun, GifItem, GifPage, GithubLogin, Harness,
    HarnessKind, NewsItem, NewsKind, PermissionStatus, ProbeResult, RepoFolder, ReviewSummary,
    ReviewView, Suggestion, SyncState, SyncStatus, TokenSource,
};

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
    }
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
        AgentLogEntry::Denied {
            at: at(3_599),
            turn: 1,
            tool: "Bash".into(),
            detail: "cargo test".into(),
        },
        AgentLogEntry::Text {
            at: at(3_598),
            turn: 1,
            text: "**Summary.** This pull request makes `TokenStore::refresh` safe to call from several tasks at once.\n\n- the token is refreshed one minute before it expires\n- a lock stops two callers from exchanging the same refresh token\n\nI could not run the tests: running commands needs permission, which arrives later.".into(),
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

/// The rows of `WhatsNew.png` ("Since you last looked, yesterday …").
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

    #[test]
    fn the_demo_agent_log_has_the_mockup_chat() {
        use clusia_protocol::AgentLogEntry;
        let turns = demo_agent_turns(1_790_000_000);
        assert_eq!(turns.len(), 2);
        let all = demo_agent_log(1_790_000_000);
        assert_eq!(all.len(), turns.iter().map(Vec::len).sum::<usize>());
        let (first, second) = (&turns[0], &turns[1]);
        assert!(
            first
                .iter()
                .any(|e| matches!(e, AgentLogEntry::Denied { tool, .. } if tool == "Bash"))
        );
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
        assert_eq!(demo_suggestions(), [suggestion.clone()]);
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
}
