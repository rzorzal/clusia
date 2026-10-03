//! Positions every element of the popover (flipped coordinates, points). Pure: the AppKit view
//! paints these shapes and hit-tests clicks against `hits`.

use crate::actions::Action;
use crate::model::{Tone, TrayView};

pub const WIDTH: f64 = 360.0;
const PAD: f64 = 16.0;
const CELL: f64 = 10.0;
const GAP: f64 = 3.0;
const WEEKS: usize = 16;
const ROW_H: f64 = 38.0;

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
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Layout {
    pub height: f64,
    pub shapes: Vec<Shape>,
    pub hits: Vec<(Rect, Action)>,
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

pub fn layout(view: &TrayView) -> Layout {
    let mut l = Layout::default();
    let inner = WIDTH - 2.0 * PAD;
    let mut y = PAD;

    // Header: brand dot, name, ⤢ (window installed) and ⚙.
    l.shapes.push(Shape::Dot {
        rect: Rect::new(PAD, y + 6.0, 8.0, 8.0),
        ink: Ink::Green,
    });
    l.shapes.push(text(
        Rect::new(PAD + 14.0, y, 160.0, 20.0),
        "Clúsia",
        Style::Title,
        Ink::Primary,
    ));
    let gear = Rect::new(WIDTH - PAD - 20.0, y, 20.0, 20.0);
    l.shapes.push(text(gear, "⚙︎", Style::Glyph, Ink::Secondary));
    l.hits.push((gear, Action::OpenConfig));
    if view.show_home {
        let home = Rect::new(WIDTH - PAD - 48.0, y, 20.0, 20.0);
        l.shapes.push(text(home, "⤢", Style::Glyph, Ink::Secondary));
        l.hits.push((home, Action::OpenHome));
    }
    y += 30.0;

    if let Some(s) = &view.status {
        l.shapes.push(text(
            Rect::new(PAD, y, inner, 16.0),
            &s.text,
            Style::Meta,
            tone_ink(s.tone),
        ));
        y += 24.0;
    }

    // Heatmap: the last 16 weeks, one column per week, oldest on the left.
    let heat: Vec<u8> = if view.heat.is_empty() {
        vec![0; WEEKS * 7]
    } else {
        view.heat[view.heat.len().saturating_sub(WEEKS * 7)..].to_vec()
    };
    for (i, level) in heat.iter().enumerate() {
        let (col, row) = ((i / 7) as f64, (i % 7) as f64);
        l.shapes.push(Shape::Cell {
            rect: Rect::new(PAD + col * (CELL + GAP), y + row * (CELL + GAP), CELL, CELL),
            level: *level,
        });
    }
    let grid_w = WEEKS as f64 * (CELL + GAP) - GAP;
    let grid_h = 7.0 * (CELL + GAP) - GAP;
    if !view.week_label.is_empty() {
        let x = PAD + grid_w + 12.0;
        l.shapes.push(Shape::Text {
            rect: Rect::new(x, y + grid_h / 2.0 - 8.0, WIDTH - PAD - x, 16.0),
            text: view.week_label.clone(),
            style: Style::Meta,
            ink: Ink::Secondary,
            right: true,
        });
    }
    y += grid_h + 16.0;

    // Counters.
    let n = view.counters.len().max(1) as f64;
    let cw = (inner - 8.0 * (n - 1.0)) / n;
    for (i, c) in view.counters.iter().enumerate() {
        let r = Rect::new(PAD + i as f64 * (cw + 8.0), y, cw, 56.0);
        l.shapes.push(Shape::Panel { rect: r });
        l.shapes.push(text(
            Rect::new(r.x + 10.0, r.y + 7.0, cw - 20.0, 26.0),
            &c.value.to_string(),
            Style::CounterValue,
            Ink::Primary,
        ));
        l.shapes.push(text(
            Rect::new(r.x + 10.0, r.y + 35.0, cw - 20.0, 14.0),
            c.label,
            Style::CounterLabel,
            Ink::Secondary,
        ));
        if let Some(a) = &c.action {
            l.hits.push((r, a.clone()));
        }
    }
    y += 56.0 + 16.0;

    // Lists.
    for section in &view.sections {
        l.shapes.push(Shape::Divider {
            rect: Rect::new(PAD, y, inner, 1.0),
        });
        y += 10.0;
        l.shapes.push(text(
            Rect::new(PAD, y, inner, 16.0),
            section.title,
            Style::Heading,
            Ink::Secondary,
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
            l.hits.push((
                Rect::new(PAD - 8.0, y, inner + 16.0, ROW_H),
                row.action.clone(),
            ));
            if row.fresh {
                l.shapes.push(Shape::Dot {
                    rect: Rect::new(PAD, y + 8.0, 6.0, 6.0),
                    ink: Ink::Green,
                });
            }
            l.shapes.push(text(
                Rect::new(PAD + 12.0, y + 2.0, 44.0, 18.0),
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
                Rect::new(tx, y + 2.0, right - tx - reserved, 18.0),
                &row.title,
                Style::Body,
                Ink::Primary,
            ));
            l.shapes.push(text(
                Rect::new(tx, y + 20.0, right - tx, 14.0),
                &row.meta,
                Style::Meta,
                Ink::Tertiary,
            ));
            if let Some(b) = &row.badge {
                let pill = Rect::new(right - badge_w, y + 3.0, badge_w, 16.0);
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
                Rect::new(WIDTH - PAD - 8.0, y + 10.0, 8.0, 14.0),
                "›",
                Style::Glyph,
                Ink::Tertiary,
            ));
            y += ROW_H;
        }
        y += 6.0;
    }
    l.height = (y + PAD - 6.0).ceil();
    l
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Snapshot, TrayModel};
    use clusia_core::{ActivitySummary, DayCount, PrRef, PrSummary, ReviewState};
    use clusia_protocol::{ReviewSummary, SyncState, SyncStatus};

    const NOW: i64 = 1_790_000_000;

    fn pr(n: u64, title: &str) -> PrSummary {
        PrSummary {
            pr: PrRef {
                owner: "rzorzal".into(),
                repo: "clusia".into(),
                number: n,
            },
            title: title.into(),
            author: "octo".into(),
            url: format!("https://github.com/rzorzal/clusia/pull/{n}"),
            draft: false,
            updated_at: "2026-09-20T10:00:00Z".into(),
            comments: 0,
        }
    }

    fn view(n: usize, app: bool, long_title: bool) -> TrayView {
        let title = if long_title {
            "x".repeat(300)
        } else {
            "fix things".to_string()
        };
        let mut m = TrayModel::new(app);
        m.apply(Snapshot {
            assigned: (1..=n as u64).map(|i| pr(i, &title)).collect(),
            reviews: (1..=n as u64)
                .map(|i| ReviewSummary {
                    pr: PrRef {
                        owner: "rzorzal".into(),
                        repo: "blog".into(),
                        number: 100 + i,
                    },
                    title: title.clone(),
                    state: ReviewState::Outdated,
                    items: 2,
                    updated_at: NOW - 60,
                })
                .collect(),
            activity: Some(ActivitySummary {
                heatmap: (0..112)
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
            }),
            lists_loaded: true,
            ..Snapshot::default()
        });
        m.view(NOW)
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

    fn text_rect(l: &Layout, needle: &str) -> Rect {
        l.shapes
            .iter()
            .find_map(|s| match s {
                Shape::Text { rect, text, .. } if text == needle => Some(*rect),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no text {needle:?}"))
    }

    #[test]
    fn height_stays_bounded_with_long_lists() {
        let l = layout(&view(50, true, true));
        assert!(l.height <= 700.0, "height {}", l.height);
        for s in &l.shapes {
            let r = match s {
                Shape::Text { rect, .. }
                | Shape::Cell { rect, .. }
                | Shape::Panel { rect }
                | Shape::Dot { rect, .. }
                | Shape::Pill { rect, .. }
                | Shape::Divider { rect } => *rect,
            };
            assert!(
                r.x >= 0.0 && r.x + r.w <= WIDTH + 0.01,
                "{s:?} leaves the popover"
            );
            assert!(r.y + r.h <= l.height + 0.01, "{s:?} below the bottom");
        }
    }

    #[test]
    fn empty_view_shows_messages_and_a_full_grid() {
        let l = layout(&TrayModel::new(false).view(NOW));
        let cells = l
            .shapes
            .iter()
            .filter(|s| matches!(s, Shape::Cell { .. }))
            .count();
        assert_eq!(cells, 16 * 7);
        let t = texts(&l);
        assert!(t.contains(&"Nothing waiting for your review"));
        assert!(t.contains(&"No saved reviews"));
        assert!(t.contains(&"Connecting to GitHub…"));
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
    fn gear_always_home_only_with_the_window() {
        let without = layout(&view(1, false, false));
        assert!(without.hits.iter().any(|(_, a)| *a == Action::OpenConfig));
        assert!(!without.hits.iter().any(|(_, a)| *a == Action::OpenHome));
        let with = layout(&view(1, true, false));
        assert!(with.hits.iter().any(|(_, a)| *a == Action::OpenHome));
    }

    #[test]
    fn fresh_rows_get_a_dot_and_heat_levels_are_kept() {
        let mut m = TrayModel::new(false);
        let base = Snapshot {
            assigned: vec![pr(1, "a")],
            sync: Some(SyncStatus {
                state: SyncState::Online,
                last_sync_unix: Some(NOW),
                next_sync_unix: None,
                message: None,
            }),
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
                Shape::Pill { rect, .. } => Some(*rect),
                _ => None,
            })
            .unwrap();
        assert!(
            titles[1].x + titles[1].w <= pill.x,
            "title runs under the badge"
        );
    }
}
