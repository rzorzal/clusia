//! Home (spec §7.2, mockup `Main.png`):
//! - the activity heatmap, quick stats, the filter field and repository chips;
//! - the Assigned / Mine / Saved lists with sort and pages.
//!
//! Filter, chip and sorts are saved in the daemon config (`lists.*`), shared with the tray.

use std::collections::HashMap;

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::text::EditableText;
use bevy::ui_widgets::{Activate, Button as WidgetButton, ScrollArea, observe};
use clusia_core::time::parse_rfc3339;
use clusia_core::{DayCount, ListSort, PrRef, ReviewState};
use clusia_protocol::{ReviewSummary, SyncState, SyncStatus, WindowTarget};
use clusia_view::heatmap::levels;
use clusia_view::lists::{
    Entry, RepoChip, clamp_page, is_saved, narrow, page_count, repo_chips, step_page,
};
use clusia_view::status::{Tone, format_age, status_line};

use crate::bridge::{Asks, Model, set_config};
use crate::clock::Clock;
use crate::fonts::UiFonts;
use crate::nav::{HomeScreen, Nav, NavSystems, Section};
use crate::snapshot::Snapshot;
use crate::theme::Swatch;
use crate::ui::kit::{
    Field, FieldCommitted, Fill, HoverFill, Stroke, Type, Variant, button, card, chip, panel, text,
    text_field,
};

pub const PAGE_SIZE: usize = 6;
pub const WEEKS: usize = 26;

#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ListId {
    Assigned,
    Mine,
    Saved,
}

/// Home's local state.
#[derive(Resource, Debug, Default)]
pub struct HomeState {
    /// The filter as typed (saved to `lists.filter` on Enter or blur).
    pub query: String,
    /// `query` was taken from the config once a snapshot arrived.
    pub seeded: bool,
    pub pages: HashMap<ListId, usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stat {
    pub value: String,
    pub unit: &'static str,
    pub label: &'static str,
    pub detail: String,
    pub accent: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RowView {
    pub pr: PrRef,
    pub number: String,
    pub title: String,
    pub meta: String,
    /// Updated within the last hour.
    pub dot: bool,
    /// Text and whether it is a warning.
    pub badge: Option<(&'static str, bool)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ListView {
    pub id: ListId,
    pub title: &'static str,
    /// Rows after filtering, across pages.
    pub total: usize,
    pub sort: ListSort,
    pub rows: Vec<RowView>,
    pub page: usize,
    pub pages: usize,
    pub empty: &'static str,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HomeView {
    pub headline: String,
    /// Text and whether it is a warning.
    pub sync: (String, bool),
    pub notice: Option<String>,
    /// `WEEKS * 7` levels 0–4, oldest first, one column per week.
    pub heat: Vec<u8>,
    /// Week column and month name where a month starts.
    pub months: Vec<(usize, &'static str)>,
    pub stats: Vec<Stat>,
    pub chips: Vec<RepoChip>,
    pub lists: Vec<ListView>,
}

/// "1 minute", "5 hours", "8 days".
pub fn long_age(secs: i64) -> String {
    let s = secs.max(0);
    let (n, unit) = if s < 3600 {
        ((s / 60).max(1), "minute")
    } else if s < 86_400 {
        (s / 3600, "hour")
    } else {
        (s / 86_400, "day")
    };
    if n == 1 {
        format!("1 {unit}")
    } else {
        format!("{n} {unit}s")
    }
}

pub fn home_view(snap: &Snapshot, state: &HomeState, now: i64) -> HomeView {
    let prefs = &snap.config.lists;
    let query = if state.seeded {
        state.query.as_str()
    } else {
        prefs.filter.as_str()
    };
    let repo = prefs.repository.as_str();
    let saved: Vec<&ReviewSummary> = snap.reviews.iter().filter(|r| is_saved(r.state)).collect();
    let oldest = snap
        .assigned
        .iter()
        .filter_map(|p| parse_rfc3339(&p.updated_at))
        .min();
    let waiting = snap.assigned.len();
    let headline = if !snap.lists_loaded {
        "Loading your pull requests…".to_string()
    } else {
        match (waiting, oldest) {
            (0, _) => "Nothing is waiting for your review.".to_string(),
            (1, Some(t)) => format!(
                "1 pull request is waiting for you. It has waited {}.",
                long_age(now - t)
            ),
            (1, None) => "1 pull request is waiting for you.".to_string(),
            (n, Some(t)) => format!(
                "{n} pull requests are waiting for you. The oldest has waited {}.",
                long_age(now - t)
            ),
            (n, None) => format!("{n} pull requests are waiting for you."),
        }
    };
    let signed_out = snap
        .sync
        .as_ref()
        .is_some_and(|s| s.state == SyncState::Unauthorized)
        || snap.auth.as_ref().is_some_and(|a| a.source.is_none());
    let notice = signed_out.then(|| {
        "Not signed in to GitHub. Open Config › Git server to sign in with the GitHub CLI or a token."
            .to_string()
    });

    let days = WEEKS * 7;
    let (heat, months) = match &snap.activity {
        Some(a) => {
            let recent = &a.heatmap[a.heatmap.len().saturating_sub(days)..];
            let pad = days - recent.len();
            let mut heat = vec![0; pad];
            heat.extend(levels(recent));
            (heat, month_labels(recent, pad))
        }
        None => (vec![0; days], Vec::new()),
    };

    let avg = snap.activity.as_ref().and_then(|a| a.avg_review_secs);
    let (avg_value, avg_unit, avg_detail) = match avg {
        Some(s) if s < 3600 => (
            (s / 60).max(1).to_string(),
            "m",
            "from first open to publish",
        ),
        Some(s) if s < 86_400 => ((s / 3600).to_string(), "h", "from first open to publish"),
        Some(s) => ((s / 86_400).to_string(), "d", "from first open to publish"),
        None => ("–".to_string(), "", "no reviews published yet"),
    };
    let stats = vec![
        Stat {
            value: waiting.to_string(),
            unit: "",
            label: "Waiting for you",
            detail: match oldest {
                Some(t) if waiting > 0 => format!("oldest {}", long_age(now - t)),
                _ => "nothing pending".to_string(),
            },
            accent: false,
        },
        Stat {
            value: avg_value,
            unit: avg_unit,
            label: "Average time to review",
            detail: avg_detail.to_string(),
            accent: false,
        },
        Stat {
            value: snap
                .activity
                .as_ref()
                .map_or(0, |a| a.published_this_week)
                .to_string(),
            unit: "",
            label: "Reviews this week",
            detail: snap.activity.as_ref().map_or_else(
                || "no activity yet".to_string(),
                |a| format!("{} in total", a.published_total),
            ),
            accent: true,
        },
    ];

    let chips = repo_chips(
        snap.assigned
            .iter()
            .chain(&snap.mine)
            .map(|p| &p.pr)
            .chain(saved.iter().map(|r| &r.pr)),
        repo,
    );
    let filtering = !query.trim().is_empty() || !repo.is_empty();
    let list = |id: ListId, title, sort, entries: Vec<Entry>, empty, loaded: bool| {
        let narrowed = narrow(entries, query, repo, sort);
        let total = narrowed.len();
        let page = clamp_page(state.pages.get(&id).copied().unwrap_or(0), total, PAGE_SIZE);
        ListView {
            id,
            title,
            total,
            sort,
            rows: narrowed
                .iter()
                .skip(page * PAGE_SIZE)
                .take(PAGE_SIZE)
                .map(|e| row(e, now))
                .collect(),
            page,
            pages: page_count(total, PAGE_SIZE),
            empty: if !loaded {
                "Loading…"
            } else if filtering {
                "No matches"
            } else {
                empty
            },
        }
    };
    let lists = vec![
        list(
            ListId::Assigned,
            "Assigned to me",
            prefs.assigned_sort,
            snap.assigned.iter().map(Entry::Pr).collect(),
            "Nothing waiting for your review",
            snap.lists_loaded,
        ),
        list(
            ListId::Mine,
            "Mine",
            prefs.mine_sort,
            snap.mine.iter().map(Entry::Pr).collect(),
            "You have no open pull requests",
            snap.lists_loaded,
        ),
        list(
            ListId::Saved,
            "Saved reviews",
            prefs.saved_sort,
            saved.iter().map(|r| Entry::Review(r)).collect(),
            "No saved reviews",
            true,
        ),
    ];
    HomeView {
        headline,
        sync: sync_pill(snap.sync.as_ref(), &snap.config.github.host, now),
        notice,
        heat,
        months,
        stats,
        chips,
        lists,
    }
}

fn sync_pill(sync: Option<&SyncStatus>, host: &str, now: i64) -> (String, bool) {
    if let Some(s) = sync
        && s.state == SyncState::Online
    {
        return match s.last_sync_unix {
            Some(t) if now - t < 60 => (format!("Synced just now · {host}"), false),
            Some(t) => (format!("Synced {} ago · {host}", format_age(now, t)), false),
            None => (format!("Online · {host}"), false),
        };
    }
    match status_line(sync, now) {
        Some(line) => (line.text, line.tone == Tone::Warning),
        None => (format!("Online · {host}"), false),
    }
}

fn row(e: &Entry, now: i64) -> RowView {
    match e {
        Entry::Pr(p) => {
            let updated = e.updated();
            let mut meta = format!(
                "{} · @{} · {}",
                p.pr.repo,
                p.author,
                format_age(now, updated)
            );
            if p.draft {
                meta.push_str(" · draft");
            }
            RowView {
                pr: p.pr.clone(),
                number: format!("#{}", p.pr.number),
                title: p.title.clone(),
                meta,
                dot: updated > 0 && now - updated < 3600,
                badge: None,
            }
        }
        Entry::Review(r) => {
            let comments = if r.items == 1 {
                "1 comment".to_string()
            } else {
                format!("{} comments", r.items)
            };
            RowView {
                pr: r.pr.clone(),
                number: format!("#{}", r.pr.number),
                title: r.title.clone(),
                meta: format!(
                    "{} · {comments} · {}",
                    r.pr.repo,
                    format_age(now, r.updated_at)
                ),
                dot: false,
                badge: match r.state {
                    ReviewState::Outdated => Some(("outdated", true)),
                    ReviewState::Revalidated => Some(("revalidated", false)),
                    _ => None,
                },
            }
        }
    }
}

/// Where each month starts, by week column. A first label that would crowd the second one
/// (fewer than 3 columns apart) is dropped.
fn month_labels(days: &[DayCount], pad: usize) -> Vec<(usize, &'static str)> {
    const NAMES: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mut marks: Vec<(usize, &'static str)> = Vec::new();
    let mut last = None;
    for col in 0..WEEKS {
        let i = col * 7;
        if i < pad {
            continue;
        }
        let Some(day) = days.get(i - pad) else { break };
        let Some(month) = day
            .date
            .get(5..7)
            .and_then(|m| m.parse::<usize>().ok())
            .filter(|m| (1..=12).contains(m))
        else {
            continue;
        };
        if last != Some(month) {
            marks.push((col, NAMES[month - 1]));
            last = Some(month);
        }
    }
    if marks.len() > 1 && marks[1].0 - marks[0].0 < 3 {
        marks.remove(0);
    }
    marks
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct HomeRow(pub PrRef);

#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortButton(pub ListId);

#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageButton(pub ListId, pub i8);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ChipButton(pub Option<String>);

#[derive(Component, Debug)]
pub struct SearchField;

#[derive(Component, Debug)]
pub struct NoticeButton;

#[derive(Component, Default)]
struct HeaderPart(Option<(String, (String, bool))>);

#[derive(Component, Default)]
struct NoticePart(Option<Option<String>>);

#[derive(Component, Default)]
struct HeatPart(Option<(Vec<u8>, Vec<(usize, &'static str)>)>);

#[derive(Component, Default)]
struct StatsPart(Option<Vec<Stat>>);

#[derive(Component, Default)]
struct ChipsPart(Option<Vec<RepoChip>>);

#[derive(Component)]
struct ListPart {
    id: ListId,
    built: Option<ListView>,
}

pub struct HomePlugin;

impl Plugin for HomePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<HomeState>().add_systems(
            Update,
            (
                seed_query,
                build_home,
                live_query,
                commit_filter,
                rebuild_home,
            )
                .chain()
                .after(NavSystems),
        );
    }
}

/// Takes the saved filter from the first snapshot.
fn seed_query(
    model: Res<Model>,
    mut state: ResMut<HomeState>,
    mut fields: Query<(&mut EditableText, &mut Field), With<SearchField>>,
) {
    if state.seeded || model.snapshot.daemon_version.is_empty() {
        return;
    }
    state.seeded = true;
    state.query = model.snapshot.config.lists.filter.clone();
    for (mut editable, mut field) in &mut fields {
        let mut fresh = EditableText::new(&state.query);
        fresh.allow_newlines = false;
        *editable = fresh;
        field.committed = state.query.clone();
    }
}

fn build_home(
    mut commands: Commands,
    screens: Query<Entity, Added<HomeScreen>>,
    fonts: Res<UiFonts>,
    state: Res<HomeState>,
) {
    for screen in &screens {
        commands.entity(screen).with_children(|p| {
            p.spawn((
                Node {
                    width: percent(100),
                    flex_direction: FlexDirection::Column,
                    row_gap: px(20),
                    padding: UiRect::axes(px(40), px(24)),
                    overflow: Overflow::scroll_y(),
                    ..default()
                },
                ScrollArea,
            ))
            .with_children(|c| {
                c.spawn((
                    Node {
                        justify_content: JustifyContent::SpaceBetween,
                        align_items: AlignItems::FlexEnd,
                        ..default()
                    },
                    HeaderPart::default(),
                ));
                c.spawn((
                    Node {
                        flex_direction: FlexDirection::Column,
                        ..default()
                    },
                    NoticePart::default(),
                ));
                c.spawn(Node {
                    column_gap: px(16),
                    ..default()
                })
                .with_children(|r| {
                    r.spawn((
                        card(Node {
                            flex_grow: 1.0,
                            flex_direction: FlexDirection::Column,
                            row_gap: px(10),
                            padding: px(18).all(),
                            ..default()
                        }),
                        HeatPart::default(),
                    ));
                    r.spawn((
                        Node {
                            width: px(360),
                            flex_shrink: 0.0,
                            flex_direction: FlexDirection::Column,
                            row_gap: px(10),
                            ..default()
                        },
                        StatsPart::default(),
                    ));
                });
                c.spawn(Node {
                    column_gap: px(12),
                    align_items: AlignItems::Center,
                    ..default()
                })
                .with_children(|r| {
                    r.spawn(text(&fonts, "Filter", Type::MUTED));
                    r.spawn((text_field(&fonts, &state.query, 360.0, false), SearchField));
                    r.spawn((
                        Node {
                            column_gap: px(8),
                            flex_wrap: FlexWrap::Wrap,
                            ..default()
                        },
                        ChipsPart::default(),
                    ));
                });
                c.spawn(Node {
                    column_gap: px(16),
                    align_items: AlignItems::FlexStart,
                    ..default()
                })
                .with_children(|r| {
                    for id in [ListId::Assigned, ListId::Mine, ListId::Saved] {
                        r.spawn((
                            card(Node {
                                flex_grow: 1.0,
                                flex_basis: px(0),
                                flex_direction: FlexDirection::Column,
                                ..default()
                            }),
                            ListPart { id, built: None },
                        ));
                    }
                });
            });
        });
    }
}

fn live_query(
    fields: Query<&EditableText, (With<SearchField>, Changed<EditableText>)>,
    mut state: ResMut<HomeState>,
) {
    for editable in &fields {
        let value = editable.value().to_string();
        if value != state.query {
            state.query = value;
            state.pages.clear();
        }
    }
}

fn commit_filter(
    mut commits: MessageReader<FieldCommitted>,
    fields: Query<(), With<SearchField>>,
    mut asks: ResMut<Asks>,
    mut model: ResMut<Model>,
) {
    for c in commits.read() {
        if fields.get(c.entity).is_ok() {
            set_config(&mut asks, &mut model, "lists.filter", c.value.clone());
        }
    }
}

fn refill(commands: &mut Commands, entity: Entity, build: impl FnOnce(&mut ChildSpawnerCommands)) {
    commands.entity(entity).despawn_related::<Children>();
    commands.entity(entity).with_children(build);
}

fn rebuild_home(
    mut commands: Commands,
    model: Res<Model>,
    state: Res<HomeState>,
    clock: Res<Clock>,
    fonts: Res<UiFonts>,
    mut headers: Query<(Entity, &mut HeaderPart)>,
    mut notices: Query<(Entity, &mut NoticePart)>,
    mut heats: Query<(Entity, &mut HeatPart)>,
    mut stats: Query<(Entity, &mut StatsPart)>,
    mut chips: Query<(Entity, &mut ChipsPart)>,
    mut lists: Query<(Entity, &mut ListPart)>,
) {
    if headers.is_empty() {
        return; // Home is not showing
    }
    let v = home_view(&model.snapshot, &state, clock.now());
    let fonts = &*fonts;
    for (e, mut part) in &mut headers {
        let built = (v.headline.clone(), v.sync.clone());
        if part.0.as_ref() == Some(&built) {
            continue;
        }
        refill(&mut commands, e, |p| header(p, fonts, &v.headline, &v.sync));
        part.0 = Some(built);
    }
    for (e, mut part) in &mut notices {
        if part.0.as_ref() == Some(&v.notice) {
            continue;
        }
        refill(&mut commands, e, |p| {
            if let Some(n) = &v.notice {
                notice(p, fonts, n);
            }
        });
        part.0 = Some(v.notice.clone());
    }
    for (e, mut part) in &mut heats {
        let built = (v.heat.clone(), v.months.clone());
        if part.0.as_ref() == Some(&built) {
            continue;
        }
        refill(&mut commands, e, |p| heatmap(p, fonts, &v.heat, &v.months));
        part.0 = Some(built);
    }
    for (e, mut part) in &mut stats {
        if part.0.as_ref() == Some(&v.stats) {
            continue;
        }
        refill(&mut commands, e, |p| {
            for s in &v.stats {
                stat(p, fonts, s);
            }
        });
        part.0 = Some(v.stats.clone());
    }
    for (e, mut part) in &mut chips {
        if part.0.as_ref() == Some(&v.chips) {
            continue;
        }
        refill(&mut commands, e, |p| {
            for c in &v.chips {
                p.spawn((
                    chip(fonts, &c.label, c.selected),
                    ChipButton(c.repo.clone()),
                    observe(on_chip),
                ));
            }
        });
        part.0 = Some(v.chips.clone());
    }
    for (e, mut part) in &mut lists {
        let Some(list) = v.lists.iter().find(|l| l.id == part.id) else {
            continue;
        };
        if part.built.as_ref() == Some(list) {
            continue;
        }
        refill(&mut commands, e, |p| list_card(p, fonts, list));
        part.built = Some(list.clone());
    }
}

fn header(p: &mut ChildSpawnerCommands, fonts: &UiFonts, headline: &str, sync: &(String, bool)) {
    p.spawn(Node {
        flex_direction: FlexDirection::Column,
        row_gap: px(4),
        ..default()
    })
    .with_children(|c| {
        c.spawn(text(fonts, "Your reviews", Type::TITLE.size(24.0)));
        c.spawn(text(fonts, headline.to_string(), Type::MUTED));
    });
    let (dot, ink) = if sync.1 {
        (Swatch::Orange, Swatch::Orange)
    } else {
        (Swatch::Green, Swatch::Muted)
    };
    p.spawn((
        panel(
            Node {
                padding: UiRect::axes(px(12), px(6)),
                column_gap: px(8),
                align_items: AlignItems::Center,
                border: px(1).all(),
                border_radius: BorderRadius::MAX,
                ..default()
            },
            Swatch::Surface,
        ),
        BorderColor::default(),
        Stroke(Swatch::Line),
    ))
    .with_children(|c| {
        c.spawn(panel(
            Node {
                width: px(8),
                height: px(8),
                border_radius: BorderRadius::MAX,
                ..default()
            },
            dot,
        ));
        c.spawn(text(fonts, sync.0.clone(), Type::BODY.ink(ink)));
    });
}

fn notice(p: &mut ChildSpawnerCommands, fonts: &UiFonts, message: &str) {
    p.spawn(panel(
        Node {
            padding: UiRect::axes(px(16), px(12)),
            column_gap: px(12),
            align_items: AlignItems::Center,
            border_radius: BorderRadius::all(px(8)),
            ..default()
        },
        Swatch::OrangeSoft,
    ))
    .with_children(|c| {
        c.spawn(text(
            fonts,
            message.to_string(),
            Type::BODY.ink(Swatch::Orange),
        ));
        c.spawn((
            button(fonts, "Open Git server", Variant::Secondary),
            NoticeButton,
            observe(|_: On<Activate>, mut nav: ResMut<Nav>| nav.open_section(Section::GitServer)),
        ));
    });
}

fn heatmap(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    heat: &[u8],
    months: &[(usize, &'static str)],
) {
    p.spawn(Node {
        column_gap: px(10),
        align_items: AlignItems::Baseline,
        ..default()
    })
    .with_children(|c| {
        c.spawn(text(fonts, "Review activity", Type::STRONG));
        c.spawn(text(fonts, "Last 6 months", Type::META));
    });
    p.spawn(Node {
        display: Display::Grid,
        grid_template_columns: RepeatedGridTrack::flex(WEEKS as u16, 1.0),
        grid_template_rows: RepeatedGridTrack::px(7, 11.0),
        grid_auto_flow: GridAutoFlow::Column,
        column_gap: px(4),
        row_gap: px(4),
        ..default()
    })
    .with_children(|g| {
        for &level in heat {
            g.spawn(panel(
                Node {
                    border_radius: BorderRadius::all(px(2)),
                    ..default()
                },
                Swatch::Heat(level),
            ));
        }
    });
    p.spawn(Node {
        display: Display::Grid,
        grid_template_columns: RepeatedGridTrack::flex(WEEKS as u16, 1.0),
        column_gap: px(4),
        ..default()
    })
    .with_children(|g| {
        for &(col, name) in months {
            g.spawn((
                Node {
                    grid_column: GridPlacement::start(col as i16 + 1),
                    ..default()
                },
                children![text(fonts, name, Type::META)],
            ));
        }
    });
}

fn stat(p: &mut ChildSpawnerCommands, fonts: &UiFonts, s: &Stat) {
    p.spawn(card(Node {
        padding: UiRect::axes(px(18), px(14)),
        column_gap: px(16),
        align_items: AlignItems::Center,
        flex_grow: 1.0,
        ..default()
    }))
    .with_children(|c| {
        c.spawn(Node {
            width: px(72),
            align_items: AlignItems::Baseline,
            ..default()
        })
        .with_children(|v| {
            let ink = if s.accent { Swatch::Green } else { Swatch::Fg };
            v.spawn(text(fonts, s.value.clone(), Type::NUMBER.ink(ink)));
            if !s.unit.is_empty() {
                v.spawn(text(fonts, s.unit, Type::MUTED));
            }
        });
        c.spawn(Node {
            flex_direction: FlexDirection::Column,
            row_gap: px(2),
            ..default()
        })
        .with_children(|t| {
            t.spawn(text(fonts, s.label, Type::BODY));
            t.spawn(text(fonts, s.detail.clone(), Type::META));
        });
    });
}

fn list_card(p: &mut ChildSpawnerCommands, fonts: &UiFonts, list: &ListView) {
    p.spawn((
        Node {
            padding: UiRect::axes(px(16), px(10)),
            column_gap: px(8),
            align_items: AlignItems::Center,
            border: UiRect::bottom(px(1)),
            ..default()
        },
        BorderColor::default(),
        Stroke(Swatch::Line),
    ))
    .with_children(|h| {
        h.spawn(text(fonts, list.title, Type::STRONG));
        h.spawn(text(fonts, list.total.to_string(), Type::META));
        h.spawn(Node {
            flex_grow: 1.0,
            ..default()
        });
        h.spawn((
            button(fonts, list.sort.label(), Variant::Ghost),
            SortButton(list.id),
            observe(on_sort),
        ));
    });
    if list.rows.is_empty() {
        p.spawn(Node {
            padding: px(16).all(),
            ..default()
        })
        .with_children(|e| {
            e.spawn(text(fonts, list.empty, Type::MUTED));
        });
    }
    for row in &list.rows {
        p.spawn((
            Node {
                padding: UiRect::axes(px(16), px(10)),
                column_gap: px(10),
                align_items: AlignItems::Center,
                border: UiRect::bottom(px(1)),
                ..default()
            },
            WidgetButton,
            Hovered::default(),
            TabIndex(0),
            BackgroundColor::default(),
            Fill(Swatch::Clear),
            HoverFill(Swatch::Hover),
            BorderColor::default(),
            Stroke(Swatch::Line),
            HomeRow(row.pr.clone()),
            observe(on_row),
        ))
        .with_children(|r| {
            r.spawn(panel(
                Node {
                    width: px(7),
                    height: px(7),
                    border_radius: BorderRadius::MAX,
                    flex_shrink: 0.0,
                    ..default()
                },
                if row.dot {
                    Swatch::Green
                } else {
                    Swatch::Clear
                },
            ));
            r.spawn(Node {
                flex_direction: FlexDirection::Column,
                row_gap: px(2),
                flex_grow: 1.0,
                min_width: px(0),
                ..default()
            })
            .with_children(|t| {
                t.spawn(Node {
                    column_gap: px(8),
                    ..default()
                })
                .with_children(|line| {
                    line.spawn(text(fonts, row.number.clone(), Type::MONO));
                    line.spawn(text(fonts, row.title.clone(), Type::BODY));
                });
                t.spawn(text(fonts, row.meta.clone(), Type::META));
            });
            if let Some((badge, warning)) = row.badge {
                let (fill, ink) = if warning {
                    (Swatch::OrangeSoft, Swatch::Orange)
                } else {
                    (Swatch::GreenSoft, Swatch::Green)
                };
                r.spawn(panel(
                    Node {
                        padding: UiRect::axes(px(8), px(2)),
                        border_radius: BorderRadius::MAX,
                        ..default()
                    },
                    fill,
                ))
                .with_children(|b| {
                    b.spawn(text(fonts, badge, Type::META.ink(ink)));
                });
            }
        });
    }
    p.spawn(Node {
        padding: UiRect::axes(px(16), px(8)),
        column_gap: px(8),
        justify_content: JustifyContent::FlexEnd,
        align_items: AlignItems::Center,
        ..default()
    })
    .with_children(|f| {
        f.spawn((
            button(fonts, "‹", Variant::Ghost),
            PageButton(list.id, -1),
            observe(on_page),
        ));
        f.spawn(text(
            fonts,
            format!("{} / {}", list.page + 1, list.pages),
            Type::META,
        ));
        f.spawn((
            button(fonts, "›", Variant::Ghost),
            PageButton(list.id, 1),
            observe(on_page),
        ));
    });
}

fn on_row(activate: On<Activate>, rows: Query<&HomeRow>, mut nav: ResMut<Nav>) {
    if let Ok(HomeRow(pr)) = rows.get(activate.entity) {
        nav.go(&WindowTarget::Review { pr: pr.clone() });
    }
}

fn on_sort(
    activate: On<Activate>,
    buttons: Query<&SortButton>,
    mut asks: ResMut<Asks>,
    mut model: ResMut<Model>,
    mut state: ResMut<HomeState>,
) {
    let Ok(SortButton(id)) = buttons.get(activate.entity) else {
        return;
    };
    let lists = &model.snapshot.config.lists;
    let (key, current) = match id {
        ListId::Assigned => ("lists.assigned_sort", lists.assigned_sort),
        ListId::Mine => ("lists.mine_sort", lists.mine_sort),
        ListId::Saved => ("lists.saved_sort", lists.saved_sort),
    };
    state.pages.remove(id);
    set_config(&mut asks, &mut model, key, current.next().as_str());
}

fn on_page(
    activate: On<Activate>,
    buttons: Query<&PageButton>,
    model: Res<Model>,
    clock: Res<Clock>,
    mut state: ResMut<HomeState>,
) {
    let Ok(&PageButton(id, delta)) = buttons.get(activate.entity) else {
        return;
    };
    let total = home_view(&model.snapshot, &state, clock.now())
        .lists
        .iter()
        .find(|l| l.id == id)
        .map_or(0, |l| l.total);
    let current = state.pages.get(&id).copied().unwrap_or(0);
    state
        .pages
        .insert(id, step_page(current, isize::from(delta), total, PAGE_SIZE));
}

fn on_chip(
    activate: On<Activate>,
    chips: Query<&ChipButton>,
    mut asks: ResMut<Asks>,
    mut model: ResMut<Model>,
    mut state: ResMut<HomeState>,
) {
    if let Ok(ChipButton(repo)) = chips.get(activate.entity) {
        state.pages.clear();
        let value = repo.clone().unwrap_or_default();
        set_config(&mut asks, &mut model, "lists.repository", value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::Ask;
    use crate::fixture;
    use crate::nav::Screen;
    use crate::testing::{self, NOW};
    use clusia_protocol::AuthInfo;

    fn view(snap: &Snapshot, state: &HomeState) -> HomeView {
        home_view(snap, state, NOW)
    }

    #[test]
    fn demo_home_view() {
        let v = view(&fixture::demo(NOW), &HomeState::default());
        assert_eq!(
            v.headline,
            "7 pull requests are waiting for you. The oldest has waited 8 days."
        );
        assert_eq!(v.sync, ("Synced 1m ago · github.com".to_string(), false));
        assert_eq!(v.notice, None);
        assert_eq!(v.heat.len(), WEEKS * 7);
        assert!(v.heat.iter().any(|&l| l > 0));
        assert_eq!(
            (v.stats[0].value.as_str(), v.stats[0].detail.as_str()),
            ("7", "oldest 8 days")
        );
        assert_eq!((v.stats[1].value.as_str(), v.stats[1].unit), ("45", "m"));
        assert_eq!(
            (
                v.stats[2].value.as_str(),
                v.stats[2].detail.as_str(),
                v.stats[2].accent
            ),
            ("12", "140 in total", true)
        );
        let chips: Vec<&str> = v.chips.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(chips, ["All", "blog", "clusia", "site"]);
        let [assigned, mine, saved] = &v.lists[..] else {
            panic!("three lists")
        };
        assert_eq!(
            (assigned.total, assigned.rows.len(), assigned.pages),
            (7, 6, 2)
        );
        assert_eq!(assigned.rows[0].number, "#123");
        assert_eq!(assigned.rows[0].meta, "clusia · @octo · 5m");
        assert!(assigned.rows[0].dot, "updated within the hour");
        assert!(!assigned.rows[1].dot);
        assert_eq!(mine.total, 3);
        assert_eq!(saved.rows[0].number, "#77");
        assert_eq!(saved.rows[0].badge, Some(("outdated", true)));
        assert_eq!(saved.rows[0].meta, "clusia · 2 comments · 3h");
        assert_eq!(saved.rows[1].meta, "blog · 1 comment · 1d");
    }

    #[test]
    fn filter_and_repository_narrow_every_list() {
        let mut snap = fixture::demo(NOW);
        let state = HomeState {
            query: "auth".into(),
            seeded: true,
            ..HomeState::default()
        };
        let v = view(&snap, &state);
        let totals: Vec<usize> = v.lists.iter().map(|l| l.total).collect();
        assert_eq!(totals, [1, 0, 0]);
        assert_eq!(v.lists[1].empty, "No matches");
        snap.config.lists.repository = "rzorzal/blog".into();
        let v = view(&snap, &HomeState::default());
        let totals: Vec<usize> = v.lists.iter().map(|l| l.total).collect();
        assert_eq!(totals, [1, 1, 1]);
        assert!(v.chips.iter().any(|c| c.selected && c.label == "blog"));
    }

    #[test]
    fn empty_home_explains_first_run() {
        let snap = Snapshot {
            sync: Some(SyncStatus {
                state: SyncState::Unauthorized,
                ..SyncStatus::default()
            }),
            auth: Some(AuthInfo {
                source: None,
                login: None,
                scopes: vec![],
                error: Some("no GitHub token".into()),
            }),
            lists_loaded: true,
            daemon_version: "0.1.0".into(),
            ..Snapshot::default()
        };
        let v = view(&snap, &HomeState::default());
        assert_eq!(v.headline, "Nothing is waiting for your review.");
        assert!(v.notice.as_deref().unwrap().contains("Config › Git server"));
        assert!(v.sync.1, "signed out is a warning");
        assert_eq!(v.heat, vec![0; WEEKS * 7]);
        assert!(v.months.is_empty());
        assert_eq!(
            (v.stats[1].value.as_str(), v.stats[1].detail.as_str()),
            ("–", "no reviews published yet")
        );
        let empties: Vec<&str> = v.lists.iter().map(|l| l.empty).collect();
        assert_eq!(
            empties,
            [
                "Nothing waiting for your review",
                "You have no open pull requests",
                "No saved reviews"
            ]
        );
        let loading = view(&Snapshot::default(), &HomeState::default());
        assert_eq!(loading.headline, "Loading your pull requests…");
        assert_eq!(loading.lists[0].empty, "Loading…");
    }

    #[test]
    fn months_are_labeled_once_and_apart() {
        let v = view(&fixture::demo(NOW), &HomeState::default());
        assert!(v.months.len() >= 5);
        for pair in v.months.windows(2) {
            assert!(pair[1].0 >= pair[0].0 + 3, "{:?}", v.months);
            assert_ne!(pair[0].1, pair[1].1);
        }
    }

    #[test]
    fn long_ages() {
        assert_eq!(long_age(30), "1 minute");
        assert_eq!(long_age(120), "2 minutes");
        assert_eq!(long_age(3600), "1 hour");
        assert_eq!(long_age(7300), "2 hours");
        assert_eq!(long_age(8 * 86_400), "8 days");
        assert_eq!(long_age(-5), "1 minute");
    }

    #[test]
    fn home_builds_rows_and_opens_reviews() {
        let mut app = testing::app(fixture::demo(NOW));
        testing::settle(&mut app);
        assert_eq!(testing::count::<HomeRow>(&mut app), 6 + 3 + 2);
        let row = testing::find::<HomeRow>(&mut app, |r| r.0.number == 123);
        testing::activate(&mut app, row);
        assert_eq!(
            app.world().resource::<Nav>().screen,
            Screen::Review("rzorzal/clusia#123".parse().unwrap())
        );
    }

    #[test]
    fn sort_chip_and_pages() {
        let mut app = testing::app(fixture::demo(NOW));
        testing::settle(&mut app);
        let sort = testing::find::<SortButton>(&mut app, |s| s.0 == ListId::Assigned);
        testing::activate(&mut app, sort);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::SetConfig {
                key: "lists.assigned_sort".into(),
                value: "oldest".into()
            }]
        );
        let blog =
            testing::find::<ChipButton>(&mut app, |c| c.0.as_deref() == Some("rzorzal/blog"));
        testing::activate(&mut app, blog);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::SetConfig {
                key: "lists.repository".into(),
                value: "rzorzal/blog".into()
            }]
        );
        let next = testing::find::<PageButton>(&mut app, |p| *p == PageButton(ListId::Assigned, 1));
        testing::activate(&mut app, next);
        testing::settle(&mut app);
        assert_eq!(
            testing::count::<HomeRow>(&mut app),
            1 + 3 + 2,
            "page 2 of Assigned"
        );
    }

    #[test]
    fn typing_filters_live_and_enter_saves() {
        let mut app = testing::app(fixture::demo(NOW));
        testing::settle(&mut app);
        let field = testing::find::<SearchField>(&mut app, |_| true);
        app.world_mut()
            .entity_mut(field)
            .insert(EditableText::new("auth"));
        testing::settle(&mut app);
        assert_eq!(testing::count::<HomeRow>(&mut app), 1);
        app.world_mut().write_message(FieldCommitted {
            entity: field,
            value: "auth".into(),
        });
        app.update();
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::SetConfig {
                key: "lists.filter".into(),
                value: "auth".into()
            }]
        );
    }

    #[test]
    fn signed_out_notice_opens_git_server() {
        let snap = Snapshot {
            sync: Some(SyncStatus {
                state: SyncState::Unauthorized,
                ..SyncStatus::default()
            }),
            lists_loaded: true,
            daemon_version: "0.1.0".into(),
            ..Snapshot::default()
        };
        let mut app = testing::app(snap);
        testing::settle(&mut app);
        let notice = testing::find::<NoticeButton>(&mut app, |_| true);
        testing::activate(&mut app, notice);
        assert_eq!(
            app.world().resource::<Nav>().screen,
            Screen::Config(Section::GitServer)
        );
    }
}
