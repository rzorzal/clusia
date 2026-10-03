//! Positions every element of the popover (flipped coordinates, points). Pure: the AppKit view
//! paints these shapes and hit-tests clicks against `hits`.

use crate::actions::Action;
use crate::model::{Row, Section, Tone, TrayView};

pub const WIDTH: f64 = 360.0;
const PAD: f64 = 16.0;
const GAP: f64 = 3.0;
const WEEKS: usize = 26;
const ROW_H: f64 = 34.0;
pub const TOOL_BUTTON: f64 = 32.0;
const TOOL_GAP: f64 = 2.0;
const TOOL_PAD: f64 = 3.0;
const COUNTER_H: f64 = 44.0;
const SEARCH_H: f64 = 24.0;
const CHIP_H: f64 = 22.0;
const SORT_W: f64 = 90.0;
const PAGE_ROW_H: f64 = 22.0;
const PAGE_ARROW_W: f64 = 18.0;
pub const SEARCH_PLACEHOLDER: &str = "Filter by title, repository or #number";

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub const fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x, y, w, h }
    }

    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Title,
    Heading,
    Body,
    Number,
    Meta,
    CounterValue,
    CounterLabel,
    Badge,
    Glyph,
    /// Row and page chevrons.
    Chevron,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ink {
    Primary,
    Secondary,
    Tertiary,
    Green,
    Orange,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    /// Single line, tail-truncated to `rect.w`; `right` aligns right (badges are centered).
    Text {
        rect: Rect,
        text: String,
        style: Style,
        ink: Ink,
        right: bool,
    },
    Cell {
        rect: Rect,
        level: u8,
    },
    Panel {
        rect: Rect,
    },
    Dot {
        rect: Rect,
        ink: Ink,
    },
    Pill {
        rect: Rect,
        ink: Ink,
    },
    Divider {
        rect: Rect,
    },
    /// The rounded background of the header toolbar.
    Group {
        rect: Rect,
    },
    /// A raised (active) toolbar button.
    Raised {
        rect: Rect,
    },
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Layout {
    pub height: f64,
    pub shapes: Vec<Shape>,
    pub hits: Vec<(Rect, Action)>,
    pub search: Option<Rect>,
}

impl Layout {
    pub fn hit(&self, x: f64, y: f64) -> Option<&Action> {
        self.hits
            .iter()
            .find(|(r, _)| r.contains(x, y))
            .map(|(_, a)| a)
    }
}

fn tone_ink(t: Tone) -> Ink {
    match t {
        Tone::Neutral => Ink::Secondary,
        Tone::Warning => Ink::Orange,
    }
}

fn text(rect: Rect, s: &str, style: Style, ink: Ink) -> Shape {
    Shape::Text {
        rect,
        text: s.to_string(),
        style,
        ink,
        right: false,
    }
}

fn right_text(rect: Rect, s: &str, style: Style, ink: Ink) -> Shape {
    Shape::Text {
        rect,
        text: s.to_string(),
        style,
        ink,
        right: true,
    }
}

pub fn layout(view: &TrayView) -> Layout {
    let mut l = Layout::default();
    let inner = WIDTH - 2.0 * PAD;
    let mut y = PAD;

    // Header: brand dot and name; a right-aligned toolbar of 32 pt buttons.
    l.shapes.push(Shape::Dot {
        rect: Rect::new(PAD, y + 9.0, 8.0, 8.0),
        ink: Ink::Green,
    });
    l.shapes.push(text(
        Rect::new(PAD + 14.0, y + 3.0, 140.0, 20.0),
        "Clúsia",
        Style::Title,
        Ink::Primary,
    ));
    let mut tools: Vec<(&str, Action)> = vec![("↻", Action::Refresh)];
    if view.show_home {
        tools.push(("⤢", Action::OpenHome));
    }
    tools.push(("⚙︎", Action::OpenConfig));
    tools.push(("⏻", Action::TurnOff));
    let n = tools.len() as f64;
    let group_w = 2.0 * TOOL_PAD + n * TOOL_BUTTON + (n - 1.0) * TOOL_GAP;
    let group = Rect::new(
        WIDTH - PAD - group_w,
        y - 4.0,
        group_w,
        TOOL_BUTTON + 2.0 * TOOL_PAD,
    );
    l.shapes.push(Shape::Group { rect: group });
    for (i, (glyph, action)) in tools.into_iter().enumerate() {
        let r = Rect::new(
            group.x + TOOL_PAD + i as f64 * (TOOL_BUTTON + TOOL_GAP),
            group.y + TOOL_PAD,
            TOOL_BUTTON,
            TOOL_BUTTON,
        );
        let active = view.syncing && action == Action::Refresh;
        if active {
            l.shapes.push(Shape::Raised { rect: r });
        }
        l.shapes.push(text(
            Rect::new(r.x, r.y + 5.0, r.w, r.h - 5.0),
            glyph,
            Style::Glyph,
            if active { Ink::Green } else { Ink::Secondary },
        ));
        l.hits.push((r, action));
    }
    y += group.h + 4.0;

    if let Some(s) = &view.status {
        l.shapes.push(text(
            Rect::new(PAD, y, inner, 16.0),
            &s.text,
            Style::Meta,
            tone_ink(s.tone),
        ));
        y += 24.0;
    }

    // Caption over the heatmap: range and weekly count on the left, sync state on the right.
    let left = if view.week_label.is_empty() {
        "Last 6 months".to_string()
    } else {
        format!("Last 6 months · {}", view.week_label)
    };
    l.shapes.push(text(
        Rect::new(PAD, y, inner * 0.62, 16.0),
        &left,
        Style::Meta,
        Ink::Tertiary,
    ));
    if let Some(c) = &view.sync_caption {
        let ink = if view.syncing {
            Ink::Green
        } else {
            tone_ink(c.tone)
        };
        l.shapes.push(right_text(
            Rect::new(PAD + inner * 0.62, y, inner * 0.38, 16.0),
            &c.text,
            Style::Meta,
            ink,
        ));
    }
    y += 18.0;

    // Heatmap: the last 26 weeks across the full width, one column per week, oldest on the left.
    let days = WEEKS * 7;
    let heat: Vec<u8> = if view.heat.is_empty() {
        vec![0; days]
    } else {
        let recent = &view.heat[view.heat.len().saturating_sub(days)..];
        // Short history: pad the oldest days so the newest stays in the last column.
        let mut h = vec![0; days - recent.len()];
        h.extend_from_slice(recent);
        h
    };
    let cell = (inner - (WEEKS as f64 - 1.0) * GAP) / WEEKS as f64;
    for (i, level) in heat.iter().enumerate() {
        let (col, row) = ((i / 7) as f64, (i % 7) as f64);
        l.shapes.push(Shape::Cell {
            rect: Rect::new(PAD + col * (cell + GAP), y + row * (cell + GAP), cell, cell),
            level: *level,
        });
    }
    y += 7.0 * (cell + GAP) - GAP + 14.0;

    // Counters.
    let n = view.counters.len().max(1) as f64;
    let cw = (inner - 8.0 * (n - 1.0)) / n;
    for (i, c) in view.counters.iter().enumerate() {
        let r = Rect::new(PAD + i as f64 * (cw + 8.0), y, cw, COUNTER_H);
        l.shapes.push(Shape::Panel { rect: r });
        l.shapes.push(text(
            Rect::new(r.x + 10.0, r.y + 6.0, cw * 0.5, 24.0),
            &c.value.to_string(),
            Style::CounterValue,
            Ink::Primary,
        ));
        l.shapes.push(right_text(
            Rect::new(r.x + cw * 0.4, r.y + 14.0, cw * 0.6 - 10.0, 14.0),
            c.label,
            Style::CounterLabel,
            Ink::Secondary,
        ));
        if let Some(a) = &c.action {
            l.hits.push((r, a.clone()));
        }
    }
    y += COUNTER_H + 14.0;

    // Search: the native field sits here (offscreen renders draw a placeholder instead).
    l.search = Some(Rect::new(PAD, y, inner, SEARCH_H));
    y += SEARCH_H + 10.0;

    // Repository chips: one row; chips that don't fit are left out.
    if !view.chips.is_empty() {
        let mut x = PAD;
        for chip in &view.chips {
            let w = 14.0 + 6.5 * chip.label.chars().count() as f64;
            if x + w > WIDTH - PAD {
                break;
            }
            let r = Rect::new(x, y, w, CHIP_H);
            if chip.selected {
                l.shapes.push(Shape::Pill {
                    rect: r,
                    ink: Ink::Green,
                });
            } else {
                l.shapes.push(Shape::Panel { rect: r });
            }
            l.shapes.push(text(
                Rect::new(r.x + 7.0, r.y + 4.0, w - 14.0 + 2.0, 14.0),
                &chip.label,
                Style::Meta,
                if chip.selected {
                    Ink::Primary
                } else {
                    Ink::Secondary
                },
            ));
            l.hits.push((r, chip.action.clone()));
            x += w + 6.0;
        }
        y += CHIP_H + 10.0;
    }

    // Lists.
    for section in &view.sections {
        l.shapes.push(Shape::Divider {
            rect: Rect::new(PAD, y, inner, 1.0),
        });
        y += 10.0;
        l.shapes.push(text(
            Rect::new(PAD, y, inner - SORT_W, 16.0),
            section.title,
            Style::Heading,
            Ink::Secondary,
        ));
        let sort = Rect::new(WIDTH - PAD - SORT_W, y, SORT_W, 16.0);
        l.shapes.push(right_text(
            sort,
            &format!("{} ▾", section.sort_label),
            Style::Meta,
            Ink::Secondary,
        ));
        l.hits.push((
            Rect::new(sort.x, sort.y - 3.0, sort.w, sort.h + 6.0),
            Action::CycleSort(section.id),
        ));
        y += 22.0;
        if section.rows.is_empty() {
            l.shapes.push(text(
                Rect::new(PAD + 12.0, y, inner - 12.0, 16.0),
                section.empty,
                Style::Meta,
                Ink::Tertiary,
            ));
            y += 26.0;
        }
        for row in &section.rows {
            row_shapes(&mut l, row, y);
            y += ROW_H;
        }
        if section.pages > 1 {
            page_row(&mut l, section, y);
            y += PAGE_ROW_H;
        }
        y += 6.0;
    }
    l.height = (y + PAD - 6.0).ceil();
    l
}

fn row_shapes(l: &mut Layout, row: &Row, y: f64) {
    let inner = WIDTH - 2.0 * PAD;
    l.hits.push((
        Rect::new(PAD - 8.0, y, inner + 16.0, ROW_H),
        row.action.clone(),
    ));
    if row.fresh {
        l.shapes.push(Shape::Dot {
            rect: Rect::new(PAD, y + 7.0, 6.0, 6.0),
            ink: Ink::Green,
        });
    }
    l.shapes.push(text(
        Rect::new(PAD + 12.0, y + 1.0, 44.0, 18.0),
        &row.number,
        Style::Number,
        Ink::Secondary,
    ));
    let right = WIDTH - PAD - 14.0;
    let tx = PAD + 56.0;
    let badge_w = row
        .badge
        .as_ref()
        .map(|b| 12.0 + 6.0 * b.text.chars().count() as f64)
        .unwrap_or(0.0);
    let reserved = if badge_w > 0.0 { badge_w + 6.0 } else { 0.0 };
    l.shapes.push(text(
        Rect::new(tx, y + 1.0, right - tx - reserved, 18.0),
        &row.title,
        Style::Body,
        Ink::Primary,
    ));
    l.shapes.push(text(
        Rect::new(tx, y + 18.0, right - tx, 14.0),
        &row.meta,
        Style::Meta,
        Ink::Tertiary,
    ));
    if let Some(b) = &row.badge {
        let pill = Rect::new(right - badge_w, y + 2.0, badge_w, 16.0);
        l.shapes.push(Shape::Pill {
            rect: pill,
            ink: tone_ink(b.tone),
        });
        l.shapes.push(text(
            Rect::new(pill.x, pill.y + 2.0, pill.w, 12.0),
            b.text,
            Style::Badge,
            tone_ink(b.tone),
        ));
    }
    l.shapes.push(text(
        Rect::new(WIDTH - PAD - 10.0, y + 8.0, 10.0, 18.0),
        "›",
        Style::Chevron,
        Ink::Tertiary,
    ));
}

/// `‹  2 / 3  ›`, right-aligned; an arrow at either end of the list is dimmed and inert.
fn page_row(l: &mut Layout, section: &Section, y: f64) {
    let caption = format!("{} / {}", section.page + 1, section.pages);
    let label_w = 5.0 * caption.chars().count() as f64 + 4.0;
    let next = Rect::new(WIDTH - PAD - PAGE_ARROW_W, y, PAGE_ARROW_W, PAGE_ROW_H);
    let label = Rect::new(next.x - label_w, y + 3.0, label_w, 16.0);
    let prev = Rect::new(label.x - PAGE_ARROW_W, y, PAGE_ARROW_W, PAGE_ROW_H);
    let mut arrow = |r: Rect, glyph: &str, enabled: bool, delta: i8| {
        l.shapes.push(text(
            Rect::new(r.x, r.y + 2.0, r.w, r.h - 4.0),
            glyph,
            Style::Chevron,
            if enabled {
                Ink::Secondary
            } else {
                Ink::Tertiary
            },
        ));
        if enabled {
            l.hits.push((r, Action::Page(section.id, delta)));
        }
    };
    arrow(prev, "‹", section.page > 0, -1);
    arrow(next, "›", section.page + 1 < section.pages, 1);
    l.shapes.push(Shape::Text {
        rect: label,
        text: caption,
        style: Style::Meta,
        ink: Ink::Secondary,
        right: true,
    });
}

/// The search row as offscreen renders show it (no native field there).
pub fn search_placeholder(r: Rect) -> Vec<Shape> {
    vec![
        Shape::Panel { rect: r },
        text(
            Rect::new(r.x + 10.0, r.y + 5.0, r.w - 20.0, 14.0),
            SEARCH_PLACEHOLDER,
            Style::Meta,
            Ink::Tertiary,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ListId, Snapshot, StatusLine, Tone, TrayModel};
    use clusia_core::{ActivitySummary, DayCount, PrRef, PrSummary, ReviewState};
    use clusia_protocol::{ReviewSummary, SyncState, SyncStatus};

    const NOW: i64 = 1_790_000_000;

    fn pr_in(repo: &str, n: u64, title: &str) -> PrSummary {
        PrSummary {
            pr: PrRef {
                owner: "rzorzal".into(),
                repo: repo.into(),
                number: n,
            },
            title: title.into(),
            author: "octo".into(),
            url: format!("https://github.com/rzorzal/{repo}/pull/{n}"),
            draft: false,
            updated_at: "2026-09-20T10:00:00Z".into(),
            comments: 0,
        }
    }

    fn pr(n: u64, title: &str) -> PrSummary {
        pr_in("clusia", n, title)
    }

    fn online() -> SyncStatus {
        SyncStatus {
            state: SyncState::Online,
            last_sync_unix: Some(NOW),
            next_sync_unix: None,
            message: None,
            paused: false,
        }
    }

    /// `n` assigned PRs and `n` outdated saved reviews spread over `repos` repositories.
    fn model(n: usize, app: bool, long_title: bool, repos: usize) -> TrayModel {
        let title = if long_title {
            "x".repeat(300)
        } else {
            "fix things".to_string()
        };
        let repo = |i: u64| format!("repo{}", i as usize % repos.max(1));
        let mut m = TrayModel::new(app);
        m.apply(Snapshot {
            assigned: (1..=n as u64).map(|i| pr_in(&repo(i), i, &title)).collect(),
            reviews: (1..=n as u64)
                .map(|i| ReviewSummary {
                    pr: PrRef {
                        owner: "rzorzal".into(),
                        repo: repo(i),
                        number: 100 + i,
                    },
                    title: title.clone(),
                    state: ReviewState::Outdated,
                    items: 2,
                    updated_at: NOW - 60,
                })
                .collect(),
            activity: Some(ActivitySummary {
                heatmap: (0..182)
                    .map(|i| DayCount {
                        date: "2026-10-01".into(),
                        count: i % 5,
                    })
                    .collect(),
                published_this_week: 12,
                published_total: 40,
                avg_review_secs: None,
            }),
            sync: Some(SyncStatus {
                state: SyncState::Unauthorized,
                last_sync_unix: Some(NOW),
                next_sync_unix: None,
                message: None,
                paused: false,
            }),
            lists_loaded: true,
            ..Snapshot::default()
        });
        m
    }

    fn view(n: usize, app: bool, long_title: bool) -> TrayView {
        model(n, app, long_title, 1).view(NOW)
    }

    fn texts(l: &Layout) -> Vec<&str> {
        l.shapes
            .iter()
            .filter_map(|s| match s {
                Shape::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn text_rects(l: &Layout, needle: &str) -> Vec<Rect> {
        l.shapes
            .iter()
            .filter_map(|s| match s {
                Shape::Text { rect, text, .. } if text == needle => Some(*rect),
                _ => None,
            })
            .collect()
    }

    fn text_rect(l: &Layout, needle: &str) -> Rect {
        *text_rects(l, needle)
            .first()
            .unwrap_or_else(|| panic!("no text {needle:?}"))
    }

    fn center(r: Rect) -> (f64, f64) {
        (r.x + r.w / 2.0, r.y + r.h / 2.0)
    }

    fn shape_rect(s: &Shape) -> Rect {
        match s {
            Shape::Text { rect, .. }
            | Shape::Cell { rect, .. }
            | Shape::Panel { rect }
            | Shape::Dot { rect, .. }
            | Shape::Pill { rect, .. }
            | Shape::Divider { rect }
            | Shape::Group { rect }
            | Shape::Raised { rect } => *rect,
        }
    }

    #[test]
    fn height_stays_bounded_with_long_lists() {
        let v = model(50, true, true, 3).view(NOW);
        assert!(v.status.is_some());
        assert_eq!(v.chips.len(), 4, "All + 3 repositories");
        let l = layout(&v);
        assert!(l.height <= 730.0, "height {}", l.height);
        for s in &l.shapes {
            let r = shape_rect(s);
            assert!(
                r.x >= 0.0 && r.x + r.w <= WIDTH + 0.01,
                "{s:?} leaves the popover"
            );
            assert!(r.y >= 0.0, "{s:?} above the top");
            assert!(r.y + r.h <= l.height + 0.01, "{s:?} below the bottom");
        }
        for (r, a) in &l.hits {
            assert!(r.x >= 0.0 && r.x + r.w <= WIDTH + 0.01, "{a:?} hit leaves");
            assert!(r.y + r.h <= l.height + 0.01, "{a:?} hit below the bottom");
        }
    }

    #[test]
    fn heatmap_fills_the_width() {
        let l = layout(&view(1, false, false));
        let cells: Vec<Rect> = l
            .shapes
            .iter()
            .filter_map(|s| match s {
                Shape::Cell { rect, .. } => Some(*rect),
                _ => None,
            })
            .collect();
        assert_eq!(cells.len(), 182);
        assert!((cells[0].x - PAD).abs() < 0.01, "first x {}", cells[0].x);
        let right = cells.iter().map(|r| r.x + r.w).fold(0.0, f64::max);
        assert!((right - (WIDTH - PAD)).abs() <= 0.5, "right edge {right}");
    }

    #[test]
    fn short_history_ends_in_the_last_column() {
        let mut v = view(1, false, false);
        v.heat = vec![1, 2, 3];
        let levels: Vec<u8> = layout(&v)
            .shapes
            .iter()
            .filter_map(|s| match s {
                Shape::Cell { level, .. } => Some(*level),
                _ => None,
            })
            .collect();
        assert_eq!(levels.len(), 182);
        assert_eq!(levels[179..], [1, 2, 3]);
        assert!(levels[..179].iter().all(|l| *l == 0));
    }

    #[test]
    fn empty_view_shows_messages_and_a_full_grid() {
        let l = layout(&TrayModel::new(false).view(NOW));
        let cells = l
            .shapes
            .iter()
            .filter(|s| matches!(s, Shape::Cell { level: 0, .. }))
            .count();
        assert_eq!(cells, 26 * 7);
        let t = texts(&l);
        assert!(t.contains(&"Nothing waiting for your review"));
        assert!(t.contains(&"No saved reviews"));
        assert!(t.contains(&"Connecting to GitHub…"));
        assert!(t.contains(&"Last 6 months"));
    }

    #[test]
    fn toolbar_buttons_in_order() {
        let l = layout(&view(1, true, false));
        let tools: Vec<(Rect, Action)> = l
            .hits
            .iter()
            .filter(|(r, _)| r.y < 40.0)
            .filter(|(_, a)| {
                matches!(
                    a,
                    Action::Refresh | Action::OpenHome | Action::OpenConfig | Action::TurnOff
                )
            })
            .cloned()
            .collect();
        let actions: Vec<&Action> = tools.iter().map(|(_, a)| a).collect();
        assert_eq!(
            actions,
            [
                &Action::Refresh,
                &Action::OpenHome,
                &Action::OpenConfig,
                &Action::TurnOff
            ]
        );
        for (r, a) in &tools {
            assert!(r.w >= 28.0 && r.h >= 28.0, "{a:?} {r:?}");
        }
        assert!(
            tools.windows(2).all(|w| w[0].0.x < w[1].0.x),
            "left to right"
        );
        let group = l
            .shapes
            .iter()
            .find_map(|s| match s {
                Shape::Group { rect } => Some(*rect),
                _ => None,
            })
            .expect("a toolbar group");
        assert!(
            (group.x + group.w - (WIDTH - PAD)).abs() < 0.5,
            "right aligned"
        );
        for glyph in ["↻", "⤢", "⚙︎", "⏻"] {
            let r = text_rect(&l, glyph);
            assert!(
                group.contains(r.x + r.w / 2.0, r.y + r.h / 2.0),
                "{glyph} inside"
            );
        }
        let without = layout(&view(1, false, false));
        assert!(!without.hits.iter().any(|(_, a)| *a == Action::OpenHome));
        assert!(without.hits.iter().any(|(_, a)| *a == Action::TurnOff));
    }

    #[test]
    fn syncing_raises_refresh_and_captions_say_so() {
        let mut v = view(1, true, false);
        v.syncing = true;
        v.sync_caption = Some(StatusLine {
            text: "Syncing…".into(),
            tone: Tone::Neutral,
        });
        let l = layout(&v);
        assert!(l.shapes.iter().any(|s| matches!(s, Shape::Raised { .. })));
        assert!(texts(&l).contains(&"Syncing…"));
        let refresh = l
            .shapes
            .iter()
            .find_map(|s| match s {
                Shape::Text { text, ink, .. } if text == "↻" => Some(*ink),
                _ => None,
            })
            .unwrap();
        assert_eq!(refresh, Ink::Green);
        let caption = texts(&layout(&view(1, true, false)))
            .into_iter()
            .find(|t| t.starts_with("Last 6 months"))
            .unwrap()
            .to_string();
        assert!(caption.contains("reviews this week") || caption == "Last 6 months");
    }

    #[test]
    fn sort_control_cycles_that_list() {
        let l = layout(&view(3, false, false));
        let controls = text_rects(&l, "Updated ▾");
        assert_eq!(controls.len(), 2, "one per list");
        let (x, y) = center(controls[1]);
        assert_eq!(l.hit(x, y), Some(&Action::CycleSort(ListId::Saved)));
        let (x, y) = center(controls[0]);
        assert_eq!(l.hit(x, y), Some(&Action::CycleSort(ListId::Assigned)));
    }

    #[test]
    fn page_controls_only_when_needed() {
        let mut m = model(10, false, false, 1);
        let l = layout(&m.view(NOW));
        assert_eq!(text_rects(&l, "1 / 3").len(), 2, "both lists have 10");
        let next = text_rects(&l, "›").into_iter().find(|r| {
            let (x, y) = center(*r);
            l.hit(x, y) == Some(&Action::Page(ListId::Assigned, 1))
        });
        assert!(next.is_some(), "› moves the assigned list forward");
        for prev in text_rects(&l, "‹") {
            let (x, y) = center(prev);
            assert!(
                !matches!(l.hit(x, y), Some(Action::Page(..))),
                "‹ is disabled on the first page"
            );
        }
        m.page(ListId::Assigned, 2);
        let l = layout(&m.view(NOW));
        assert!(texts(&l).contains(&"3 / 3"));
        let back = text_rects(&l, "‹").into_iter().any(|r| {
            let (x, y) = center(r);
            l.hit(x, y) == Some(&Action::Page(ListId::Assigned, -1))
        });
        assert!(back, "‹ moves back from the last page");

        let l = layout(&view(3, false, false));
        assert!(
            !texts(&l).iter().any(|t| t.contains(" / ")),
            "{:?}",
            texts(&l)
        );
    }

    #[test]
    fn chips_are_clickable_and_never_wrap() {
        let v = model(30, false, false, 30).view(NOW);
        assert_eq!(v.chips.len(), 31);
        let l = layout(&v);
        // The selected chip is a Pill, the others are Panels; both are 22 pt tall.
        let pills: Vec<Rect> = l
            .shapes
            .iter()
            .filter_map(|s| match s {
                Shape::Pill { rect, .. } | Shape::Panel { rect } if rect.h == 22.0 => Some(*rect),
                _ => None,
            })
            .collect();
        assert!(pills.len() >= 2 && pills.len() < 31, "{}", pills.len());
        for p in &pills {
            assert!(p.x + p.w <= WIDTH - PAD + 0.01, "{p:?} overflows");
            assert_eq!(p.y, pills[0].y, "chips wrapped");
        }
        let all = text_rect(&l, "All");
        let (x, y) = center(all);
        assert_eq!(l.hit(x, y), Some(&Action::Repository(None)));
    }

    #[test]
    fn search_row_is_reserved() {
        let l = layout(&view(1, false, false));
        let s = l.search.expect("a search row");
        assert_eq!((s.x, s.w, s.h), (PAD, 328.0, 24.0));
    }

    #[test]
    fn row_click_opens_that_review() {
        let v = view(3, false, false);
        let l = layout(&v);
        let r = text_rect(&l, "#2");
        assert_eq!(
            l.hit(r.x + r.w / 2.0, r.y + r.h / 2.0),
            Some(&v.sections[0].rows[1].action)
        );
        assert_eq!(l.hit(1.0, 1.0), None, "the corner is not a button");
    }

    #[test]
    fn fresh_rows_get_a_dot() {
        let mut m = TrayModel::new(false);
        let base = Snapshot {
            assigned: vec![pr(1, "a")],
            sync: Some(online()),
            lists_loaded: true,
            ..Snapshot::default()
        };
        m.apply(base.clone());
        let mut changed = base;
        changed.assigned[0].updated_at = "2026-09-21T10:00:00Z".into();
        m.apply(changed);
        let l = layout(&m.view(NOW));
        // The header dot plus the fresh row's dot.
        assert_eq!(
            l.shapes
                .iter()
                .filter(|s| matches!(s, Shape::Dot { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn titles_keep_room_next_to_badges() {
        let l = layout(&view(1, false, true));
        let long = "x".repeat(300);
        let titles: Vec<Rect> = l
            .shapes
            .iter()
            .filter_map(|s| match s {
                Shape::Text {
                    rect,
                    text,
                    style: Style::Body,
                    ..
                } if *text == long => Some(*rect),
                _ => None,
            })
            .collect();
        assert_eq!(titles.len(), 2);
        assert!(titles.iter().all(|r| r.w >= 120.0), "{titles:?}");
        let pill = l
            .shapes
            .iter()
            .find_map(|s| match s {
                Shape::Pill { rect, .. } if rect.h < 22.0 => Some(*rect),
                _ => None,
            })
            .unwrap();
        assert!(
            titles[1].x + titles[1].w <= pill.x,
            "title runs under the badge"
        );
    }
}
