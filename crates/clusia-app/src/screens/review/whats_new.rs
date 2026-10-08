//! What's new (mockup `WhatsNew.png`): after a fresh open, what changed since the reviewer last
//! looked. A row goes where it happened (Diff or Comments); **Got it**, Esc or a row marks the
//! news seen. Without news the review is marked seen at once.

use std::collections::HashMap;

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input::ButtonInput;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::ui_widgets::{Activate, Button as WidgetButton, observe};
use clusia_core::PrRef;
use clusia_protocol::{NewsItem, NewsKind};

use crate::bridge::{Ask, Asks, Model};
use crate::clock::Clock;
use crate::fonts::UiFonts;
use crate::nav::{Nav, Screen};
use crate::review_state::{Modal, Phase, Ready, ReviewSection, ReviewTabs};
use crate::screens::home::long_age;
use crate::screens::review::ReviewSystems;
use crate::screens::review::shell::ModalFor;
use crate::theme::{Swatch, Theme};
use crate::ui::kit::{
    Clickable, Fill, HoverFill, Stroke, Tone, Type, Variant, button, panel, text,
};
use crate::ui::markdown::parse::parse;
use crate::ui::markdown::{RenderOpts, markdown_line};
use crate::ui::modal::{escape_pressed, modal_card, modal_root};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewsRow {
    pub icon: &'static str,
    pub tone: Tone,
    /// `2 new commits`
    pub what: String,
    /// The part after `: ` in the daemon's summary, if any.
    pub detail: String,
    /// `@mona, @hubot` or `You`
    pub who: String,
    /// `GitHub`, `GitHub Actions`, `Clúsia`, `CLI`
    pub source: String,
    /// Where a click goes (`None`: nowhere, e.g. checks).
    pub go: Option<ReviewSection>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhatsNewView {
    pub title: String,
    pub since: String,
    pub rows: Vec<NewsRow>,
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The client names the daemon reports, as people know them.
fn client_name(client: &str) -> String {
    match client {
        "clusia" => "CLI".into(),
        "clusia-app" => "Window".into(),
        "clusia-tray" => "Tray".into(),
        other => other.into(),
    }
}

pub fn news_row(item: &NewsItem) -> NewsRow {
    let (what, detail) = match item.summary.split_once(": ") {
        Some((what, detail)) => (capitalize(what), detail.to_string()),
        None => (capitalize(&item.summary), String::new()),
    };
    let lower = item.summary.to_lowercase();
    let (icon, tone, go) = match item.kind {
        NewsKind::Commits => ("◆", Tone::Green, Some(ReviewSection::Diff)),
        NewsKind::Comment => ("“", Tone::Neutral, Some(ReviewSection::Comments)),
        NewsKind::Review if lower.starts_with("approved") => {
            ("✓", Tone::Green, Some(ReviewSection::Comments))
        }
        NewsKind::Review => ("“", Tone::Neutral, Some(ReviewSection::Comments)),
        NewsKind::Checks if lower.contains("passed") => ("✓", Tone::Green, None),
        NewsKind::Checks if lower.contains("failed") => ("×", Tone::Orange, None),
        NewsKind::Checks => ("•", Tone::Neutral, None),
        NewsKind::Local => ("+", Tone::Neutral, Some(ReviewSection::Diff)),
        NewsKind::Moved => ("↻", Tone::Orange, Some(ReviewSection::Diff)),
    };
    let (who, source) = match item.source.strip_prefix("You via ") {
        Some(client) => ("You".to_string(), client_name(client)),
        None => (
            item.who
                .as_deref()
                .map(|w| {
                    w.split(", ")
                        .map(|p| format!("@{p}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default(),
            item.source.clone(),
        ),
    };
    NewsRow {
        icon,
        tone,
        what,
        detail,
        who,
        source,
        go,
    }
}

pub fn whats_new_view(ready: &Ready, now: i64) -> WhatsNewView {
    let review = &ready.view.review;
    WhatsNewView {
        title: format!("What's new in #{}", review.pr.number),
        since: match review.last_seen_at {
            Some(at) => format!("Since you last looked, {} ago.", long_age(now - at)),
            None => "Since you started this review.".into(),
        },
        rows: ready.news.iter().map(news_row).collect(),
    }
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct NewsRowButton {
    pub pr: PrRef,
    pub go: Option<ReviewSection>,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct GotIt(pub PrRef);

pub struct WhatsNewPlugin;

impl Plugin for WhatsNewPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            (open_after_load, show_whats_new, escape_closes)
                .chain()
                .in_set(ReviewSystems),
        );
    }
}

/// When a tab becomes freshly ready (not from the cache): opens What's new if there is news,
/// otherwise marks the review seen.
fn open_after_load(
    mut tabs: ResMut<ReviewTabs>,
    mut asks: ResMut<Asks>,
    mut fresh: Local<HashMap<PrRef, bool>>,
) {
    let mut todo: Vec<(PrRef, bool)> = Vec::new();
    for (pr, tab) in &tabs.0 {
        let now_fresh = matches!(&tab.phase, Phase::Ready(r) if r.cached_at.is_none());
        let was_fresh = fresh.insert(pr.clone(), now_fresh).unwrap_or(false);
        if now_fresh
            && !was_fresh
            && let Phase::Ready(r) = &tab.phase
        {
            todo.push((pr.clone(), !r.news.is_empty()));
        }
    }
    fresh.retain(|pr, _| tabs.0.contains_key(pr));
    for (pr, news) in todo {
        if news {
            if let Some(tab) = tabs.0.get_mut(&pr)
                && tab.ui.modal.is_none()
            {
                tab.ui.modal = Some(Modal::WhatsNew);
            }
        } else {
            asks.send(Ask::MarkSeen(pr));
        }
    }
}

fn show_whats_new(
    mut commands: Commands,
    nav: Res<Nav>,
    tabs: Res<ReviewTabs>,
    clock: Res<Clock>,
    fonts: Res<UiFonts>,
    theme: Res<Theme>,
    model: Res<Model>,
    open: Query<&ModalFor>,
) {
    let Screen::Review(pr) = &nav.screen else {
        return;
    };
    let Some(tab) = tabs.0.get(pr) else { return };
    let Phase::Ready(ready) = &tab.phase else {
        return;
    };
    if tab.ui.modal != Some(Modal::WhatsNew)
        || open
            .iter()
            .any(|m| &m.pr == pr && m.modal == Modal::WhatsNew)
    {
        return;
    }
    let view = whats_new_view(ready, clock.now());
    let opts = RenderOpts::from_config(theme.code_size, &model.snapshot.config);
    commands
        .spawn((
            modal_root(),
            ModalFor {
                pr: pr.clone(),
                modal: Modal::WhatsNew,
            },
        ))
        .with_children(|root| {
            root.spawn(modal_card(560.0))
                .with_children(|card| dialog(card, &fonts, pr, &view, &opts));
        });
}

fn dialog(
    c: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    v: &WhatsNewView,
    opts: &RenderOpts,
) {
    c.spawn(Node {
        flex_direction: FlexDirection::Column,
        row_gap: px(4),
        padding: UiRect {
            left: px(24),
            right: px(24),
            top: px(22),
            bottom: px(14),
        },
        ..default()
    })
    .with_children(|h| {
        h.spawn(text(fonts, v.title.clone(), Type::HEADING.size(17.0)));
        h.spawn(text(fonts, v.since.clone(), Type::MUTED));
    });
    for row in &v.rows {
        news_row_node(c, fonts, pr, row, opts);
    }
    c.spawn((
        panel(
            Node {
                column_gap: px(10),
                align_items: AlignItems::Center,
                padding: UiRect::axes(px(24), px(14)),
                border: UiRect::top(px(1)),
                ..default()
            },
            Swatch::Chrome,
        ),
        BorderColor::default(),
        Stroke(Swatch::Line),
    ))
    .with_children(|f| {
        f.spawn(text(fonts, "Click a row to go there.", Type::META));
        f.spawn(Node {
            flex_grow: 1.0,
            ..default()
        });
        f.spawn((
            button(fonts, "Got it", Variant::Primary),
            GotIt(pr.clone()),
            observe(on_got_it),
        ));
    });
}

fn news_row_node(
    c: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    row: &NewsRow,
    opts: &RenderOpts,
) {
    let (fill, ink) = row.tone.swatches();
    c.spawn((
        Node {
            column_gap: px(14),
            align_items: AlignItems::Center,
            padding: UiRect::axes(px(24), px(12)),
            border: UiRect::top(px(1)),
            ..default()
        },
        WidgetButton,
        Clickable,
        Hovered::default(),
        TabIndex(0),
        BackgroundColor::default(),
        Fill(Swatch::Clear),
        HoverFill(Swatch::Hover),
        BorderColor::default(),
        Stroke(Swatch::Line),
        NewsRowButton {
            pr: pr.clone(),
            go: row.go,
        },
        observe(on_row),
    ))
    .with_children(|r| {
        r.spawn(panel(
            Node {
                width: px(30),
                height: px(30),
                flex_shrink: 0.0,
                border_radius: BorderRadius::all(px(8)),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            fill,
        ))
        .with_children(|i| {
            i.spawn(text(
                fonts,
                row.icon,
                Type {
                    size: 14.0,
                    weight: 700,
                    ink,
                    mono: false,
                },
            ));
        });
        r.spawn(Node {
            flex_grow: 1.0,
            min_width: px(0),
            flex_direction: FlexDirection::Column,
            row_gap: px(2),
            ..default()
        })
        .with_children(|t| {
            t.spawn(text(fonts, row.what.clone(), Type::BODY.size(13.0)));
            if !row.detail.is_empty() {
                markdown_line(t, fonts, &parse(&row.detail), Type::META, opts, 140);
            }
        });
        r.spawn(Node {
            flex_shrink: 0.0,
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::End,
            row_gap: px(2),
            ..default()
        })
        .with_children(|w| {
            if !row.who.is_empty() {
                w.spawn(text(fonts, row.who.clone(), Type::MUTED.size(12.0)));
            }
            w.spawn(text(fonts, row.source.clone(), Type::META));
        });
    });
}

/// Closes What's new and tells the daemon the news was seen.
fn seen(tabs: &mut ReviewTabs, asks: &mut Asks, pr: &PrRef, go: Option<ReviewSection>) {
    let Some(tab) = tabs.0.get_mut(pr) else {
        return;
    };
    if tab.ui.modal != Some(Modal::WhatsNew) {
        return;
    }
    tab.ui.modal = None;
    if let Some(section) = go {
        tab.ui.section = section;
    }
    asks.send(Ask::MarkSeen(pr.clone()));
}

fn on_got_it(
    activate: On<Activate>,
    buttons: Query<&GotIt>,
    mut tabs: ResMut<ReviewTabs>,
    mut asks: ResMut<Asks>,
) {
    if let Ok(GotIt(pr)) = buttons.get(activate.entity) {
        seen(&mut tabs, &mut asks, pr, None);
    }
}

fn on_row(
    activate: On<Activate>,
    rows: Query<&NewsRowButton>,
    mut tabs: ResMut<ReviewTabs>,
    mut asks: ResMut<Asks>,
) {
    if let Ok(row) = rows.get(activate.entity) {
        seen(&mut tabs, &mut asks, &row.pr, row.go);
    }
}

fn escape_closes(
    keys: Res<ButtonInput<KeyCode>>,
    open: Query<&ModalFor>,
    mut tabs: ResMut<ReviewTabs>,
    mut asks: ResMut<Asks>,
) {
    if !escape_pressed(&keys) {
        return;
    }
    let shown: Vec<PrRef> = open
        .iter()
        .filter(|m| m.modal == Modal::WhatsNew)
        .map(|m| m.pr.clone())
        .collect();
    for pr in shown {
        seen(&mut tabs, &mut asks, &pr, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use crate::testing::{self, NOW};

    fn item(kind: NewsKind, source: &str, who: Option<&str>, summary: &str) -> NewsItem {
        NewsItem {
            kind,
            source: source.into(),
            who: who.map(String::from),
            at: NOW - 600,
            summary: summary.into(),
            url: None,
        }
    }

    #[test]
    fn rows_read_like_the_mockup() {
        let rows: Vec<NewsRow> = [
            item(NewsKind::Commits, "GitHub", Some("octo"), "2 new commits"),
            item(
                NewsKind::Comment,
                "GitHub",
                Some("mona, hubot"),
                "3 comments",
            ),
            item(NewsKind::Checks, "GitHub Actions", None, "CI passed"),
            item(
                NewsKind::Moved,
                "Clúsia",
                None,
                "Draft re-anchored: store.rs:81 → 88 · http.rs:20 no longer in the diff",
            ),
            item(NewsKind::Local, "You via clusia", None, "Item added"),
            item(NewsKind::Review, "GitHub", Some("hubot"), "approved"),
            item(NewsKind::Checks, "GitHub Actions", None, "CI failed"),
        ]
        .iter()
        .map(news_row)
        .collect();
        let short: Vec<(&str, Tone, &str, &str, &str, Option<ReviewSection>)> = rows
            .iter()
            .map(|r| {
                (
                    r.icon,
                    r.tone,
                    r.what.as_str(),
                    r.who.as_str(),
                    r.source.as_str(),
                    r.go,
                )
            })
            .collect();
        use ReviewSection::{Comments, Diff};
        assert_eq!(
            short,
            [
                (
                    "◆",
                    Tone::Green,
                    "2 new commits",
                    "@octo",
                    "GitHub",
                    Some(Diff)
                ),
                (
                    "“",
                    Tone::Neutral,
                    "3 comments",
                    "@mona, @hubot",
                    "GitHub",
                    Some(Comments)
                ),
                ("✓", Tone::Green, "CI passed", "", "GitHub Actions", None),
                (
                    "↻",
                    Tone::Orange,
                    "Draft re-anchored",
                    "",
                    "Clúsia",
                    Some(Diff)
                ),
                ("+", Tone::Neutral, "Item added", "You", "CLI", Some(Diff)),
                (
                    "✓",
                    Tone::Green,
                    "Approved",
                    "@hubot",
                    "GitHub",
                    Some(Comments)
                ),
                ("×", Tone::Orange, "CI failed", "", "GitHub Actions", None),
            ]
        );
        assert_eq!(
            rows[3].detail,
            "store.rs:81 → 88 · http.rs:20 no longer in the diff"
        );
        assert_eq!(rows[0].detail, "");
    }

    #[test]
    fn title_and_since() {
        let (mut view, news) = fixture::demo_review(NOW);
        view.review.last_seen_at = Some(NOW - 20 * 3600);
        let mut ready = Ready {
            view,
            news,
            cached_at: None,
        };
        let v = whats_new_view(&ready, NOW);
        assert_eq!(v.title, "What's new in #123");
        assert_eq!(v.since, "Since you last looked, 20 hours ago.");
        assert_eq!(v.rows.len(), ready.news.len());
        ready.view.review.last_seen_at = None;
        assert_eq!(
            whats_new_view(&ready, NOW).since,
            "Since you started this review."
        );
    }

    fn modal(app: &App, pr: &PrRef) -> Option<Modal> {
        app.world().resource::<ReviewTabs>().0[pr].ui.modal
    }

    #[test]
    fn opens_after_a_fresh_load_and_got_it_marks_seen() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, true);
        assert_eq!(modal(&app, &pr), Some(Modal::WhatsNew));
        let news = fixture::demo_review(NOW).1.len();
        assert_eq!(testing::count::<NewsRowButton>(&mut app), news);
        assert_eq!(testing::count::<ModalFor>(&mut app), 1);
        let got = testing::find::<GotIt>(&mut app, |_| true);
        testing::activate(&mut app, got);
        testing::settle(&mut app);
        assert_eq!(testing::recorded(&mut app), [Ask::MarkSeen(pr.clone())]);
        assert_eq!(modal(&app, &pr), None);
        assert_eq!(testing::count::<ModalFor>(&mut app), 0, "the modal is gone");
        testing::settle(&mut app);
        assert_eq!(modal(&app, &pr), None, "it does not come back");
    }

    #[test]
    fn without_news_the_review_is_marked_seen_at_once() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr: PrRef = "rzorzal/clusia#123".parse().unwrap();
        app.world_mut().write_message(crate::bridge::ShowRequested(
            clusia_protocol::WindowTarget::Review { pr: pr.clone() },
        ));
        testing::settle(&mut app);
        testing::recorded(&mut app);
        let (view, _) = fixture::demo_review(NOW);
        testing::tell(
            &mut app,
            crate::bridge::Tell::Opened {
                pr: pr.clone(),
                view: Box::new(view.clone()),
                news: vec![],
            },
        );
        testing::settle(&mut app);
        let asks: Vec<Ask> = testing::recorded(&mut app)
            .into_iter()
            .filter(|a| !matches!(a, Ask::AgentLog { .. }))
            .collect();
        assert_eq!(asks, [Ask::MarkSeen(pr.clone())]);
        assert_eq!(modal(&app, &pr), None);
        assert_eq!(testing::count::<NewsRowButton>(&mut app), 0);
        // A copy from the cache is not "fresh": nothing is marked.
        testing::tell(
            &mut app,
            crate::bridge::Tell::OpenedFromCache {
                pr: pr.clone(),
                view: Box::new(view),
                fetched_at: NOW - 60,
            },
        );
        testing::settle(&mut app);
        assert!(testing::recorded(&mut app).is_empty());
    }

    #[test]
    fn rows_go_there_and_escape_closes() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, true);
        let comments =
            testing::find::<NewsRowButton>(&mut app, |r| r.go == Some(ReviewSection::Comments));
        testing::activate(&mut app, comments);
        testing::settle(&mut app);
        assert_eq!(
            app.world().resource::<ReviewTabs>().0[&pr].ui.section,
            ReviewSection::Comments
        );
        // The Comments section shows the demo's GIF, which it asks the daemon for.
        let asks: Vec<Ask> = testing::recorded(&mut app)
            .into_iter()
            .filter(|a| !matches!(a, Ask::FetchMedia(_)))
            .collect();
        assert_eq!(asks, [Ask::MarkSeen(pr.clone())]);
        assert_eq!(modal(&app, &pr), None);
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .modal = Some(Modal::WhatsNew);
        testing::settle(&mut app);
        assert_eq!(testing::count::<ModalFor>(&mut app), 1, "reopened");
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::Escape);
        app.update();
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .reset_all();
        testing::settle(&mut app);
        assert_eq!(modal(&app, &pr), None);
        assert_eq!(testing::recorded(&mut app), [Ask::MarkSeen(pr)]);
        assert_eq!(testing::count::<ModalFor>(&mut app), 0);
    }

    #[test]
    fn details_render_as_one_markdown_line() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr: PrRef = "rzorzal/clusia#123".parse().unwrap();
        app.world_mut().write_message(crate::bridge::ShowRequested(
            clusia_protocol::WindowTarget::Review { pr: pr.clone() },
        ));
        testing::settle(&mut app);
        let (view, _) = fixture::demo_review(NOW);
        let news = vec![item(
            NewsKind::Comment,
            "GitHub",
            Some("mona"),
            "commented: a **big** call on `expires_at`",
        )];
        testing::tell(
            &mut app,
            crate::bridge::Tell::Opened {
                pr,
                view: Box::new(view),
                news,
            },
        );
        testing::settle(&mut app);
        assert!(testing::shows(&mut app, "a big call on expires_at"));
        assert!(!testing::shows(&mut app, "**"), "no raw markdown");
        let mut texts = app.world_mut().query::<(&Text, &TextFont)>();
        let big = texts
            .iter(app.world())
            .find(|(t, _)| t.0 == "big")
            .expect("a bold word");
        assert_eq!(big.1.weight.0, 600);
    }
}
