//! What `layout` returns. Every coordinate is in logical pixels, from the top left of the
//! diagram; the drawing starts at `(MARGIN, MARGIN)`.

use clusia_core::diagram::{Change, GroupKind, History, LineStyle, RowKey, Shape};

use crate::measure::TextStyle;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    pub fn centre(&self) -> Point {
        Point {
            x: self.x + self.w / 2.0,
            y: self.y + self.h / 2.0,
        }
    }

    /// The rectangle grown by `d` on every side (shrunk when `d` is negative).
    pub fn inflate(&self, d: f32) -> Rect {
        Rect {
            x: self.x - d,
            y: self.y - d,
            w: self.w + 2.0 * d,
            h: self.h + 2.0 * d,
        }
    }

    /// Whether the two rectangles share an area; touching edges do not count.
    pub fn intersects(&self, other: &Rect) -> bool {
        self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
}

/// Text already broken into lines. `at` is the top left of the block, `width` and `height` its
/// size, and `align` how each line sits inside the block's width.
#[derive(Clone, Debug, PartialEq)]
pub struct TextLines {
    pub lines: Vec<String>,
    pub style: TextStyle,
    pub at: Point,
    pub width: f32,
    pub height: f32,
    pub align: Align,
}

impl TextLines {
    pub fn rect(&self) -> Rect {
        Rect {
            x: self.at.x,
            y: self.at.y,
            w: self.width,
            h: self.height,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RowGeo {
    pub text: TextLines,
    pub key: Option<RowKey>,
    pub change: Option<Change>,
    pub rect: Rect,
    pub divider_above: bool,
}

/// One node as a box. A `Start` node keeps its 18 px box here, with empty text, so edges can
/// attach to it; `Geometry::start_dot` is its centre.
#[derive(Clone, Debug, PartialEq)]
pub struct BoxGeo {
    pub node: String,
    pub index: usize,
    pub rect: Rect,
    pub shape: Shape,
    pub title: TextLines,
    pub sub: Option<TextLines>,
    pub rows: Vec<RowGeo>,
    pub change: Change,
    pub history: Option<History>,
    pub anchored: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arrow {
    None,
    End,
    Both,
}

/// One edge as an orthogonal polyline that starts at the edge's `from` end and finishes at
/// its `to` end, so an `End` arrow points at the last point.
#[derive(Clone, Debug, PartialEq)]
pub struct EdgeGeo {
    pub index: usize,
    pub points: Vec<Point>,
    pub arrow: Arrow,
    pub label: Option<TextLines>,
    pub ends: [Option<TextLines>; 2],
    pub style: LineStyle,
    pub change: Change,
    pub history: Option<History>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GroupGeo {
    pub id: String,
    pub rect: Rect,
    pub label: TextLines,
    pub kind: GroupKind,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Lifeline {
    pub node: String,
    pub x: f32,
    pub top: f32,
    pub bottom: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Activation {
    pub node: String,
    pub rect: Rect,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MessageGeo {
    pub index: usize,
    pub y: f32,
    pub from_x: f32,
    pub to_x: f32,
    pub label: TextLines,
    pub reply: bool,
    pub change: Change,
    pub history: Option<History>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Geometry {
    pub size: (f32, f32),
    pub boxes: Vec<BoxGeo>,
    pub edges: Vec<EdgeGeo>,
    pub groups: Vec<GroupGeo>,
    pub lifelines: Vec<Lifeline>,
    pub activations: Vec<Activation>,
    pub messages: Vec<MessageGeo>,
    pub start_dot: Option<Point>,
    /// Edge labels and cardinalities left out because no place was free of boxes, texts and
    /// other routes.
    pub dropped_texts: u32,
}

/// Space between the parallel runs of two routes in one gap.
pub(crate) const STEP: f32 = 8.0;

/// How far two collinear runs overlap, or 0 when they are not on one line.
pub(crate) fn overlap(a: (Point, Point), b: (Point, Point)) -> f32 {
    let span = |p: f32, q: f32, r: f32, s: f32| p.max(q).min(r.max(s)) - p.min(q).max(r.min(s));
    let vertical = |s: (Point, Point)| (s.0.x - s.1.x).abs() < 1e-3;
    let horizontal = |s: (Point, Point)| (s.0.y - s.1.y).abs() < 1e-3;
    if vertical(a) && vertical(b) && (a.0.x - b.0.x).abs() < 1.0 {
        span(a.0.y, a.1.y, b.0.y, b.1.y)
    } else if horizontal(a) && horizontal(b) && (a.0.y - b.0.y).abs() < 1.0 {
        span(a.0.x, a.1.x, b.0.x, b.1.x)
    } else {
        0.0
    }
}

/// The actor end of a link of a use case diagram: the actor's node index, and whether the
/// route starts there.
pub(crate) type ActorEnd = Option<(usize, bool)>;

/// Whether two routes run along one line for more than 2 px anywhere but on a shared trunk.
/// Two links of one actor (`ends`) may share its trunk: the run from the actor's port to the
/// bus beside it, and the bus run to where each link turns off. Past those two runs, counted
/// from the actor's end, any shared run counts.
pub(crate) fn share_past_trunk(a: &[Point], b: &[Point], ends: (ActorEnd, ActorEnd)) -> bool {
    let on_trunk = |points: &[Point], i: usize, end: ActorEnd| match end {
        Some((_, at_start)) => {
            let from_actor = if at_start { i } else { points.len() - 2 - i };
            from_actor < 2
        }
        None => false,
    };
    let fan = matches!(ends, (Some((x, _)), Some((y, _))) if x == y);
    a.windows(2).enumerate().any(|(i, s)| {
        b.windows(2).enumerate().any(|(j, t)| {
            overlap((s[0], s[1]), (t[0], t[1])) > 2.0
                && !(fan && on_trunk(a, i, ends.0) && on_trunk(b, j, ends.1))
        })
    })
}

/// Whether `zone` touches the route of any edge but `own`; `routes` are (edge index, points).
pub(crate) fn crossed_by_other_route<P: AsRef<[Point]>>(
    routes: &[(usize, P)],
    own: usize,
    zone: Rect,
) -> bool {
    routes.iter().any(|(other, points)| {
        *other != own
            && points
                .as_ref()
                .windows(2)
                .any(|w| touches(w[0], w[1], zone))
    })
}

/// Whether the axis-aligned segment `a`–`b` touches the closed rectangle `r`.
pub(crate) fn touches(a: Point, b: Point, r: Rect) -> bool {
    a.x.max(b.x) >= r.x
        && a.x.min(b.x) <= r.right()
        && a.y.max(b.y) >= r.y
        && a.y.min(b.y) <= r.bottom()
}

/// Half of the 18 px start dot.
const DOT_RADIUS: f32 = 9.0;

impl Geometry {
    pub(crate) fn empty() -> Geometry {
        Geometry {
            size: (0.0, 0.0),
            boxes: Vec::new(),
            edges: Vec::new(),
            groups: Vec::new(),
            lifelines: Vec::new(),
            activations: Vec::new(),
            messages: Vec::new(),
            start_dot: None,
            dropped_texts: 0,
        }
    }

    /// The smallest rectangle holding everything drawn, or `None` for an empty drawing.
    pub(crate) fn bounds(&self) -> Option<Rect> {
        let mut acc: Option<(f32, f32, f32, f32)> = None;
        let mut grow = |x0: f32, y0: f32, x1: f32, y1: f32| {
            acc = Some(match acc {
                None => (x0, y0, x1, y1),
                Some((a, b, c, d)) => (a.min(x0), b.min(y0), c.max(x1), d.max(y1)),
            });
        };
        let mut rect = |r: Rect| grow(r.x, r.y, r.right(), r.bottom());
        for b in &self.boxes {
            rect(b.rect);
        }
        for g in &self.groups {
            rect(g.rect);
            rect(g.label.rect());
        }
        for a in &self.activations {
            rect(a.rect);
        }
        for m in &self.messages {
            rect(m.label.rect());
        }
        let mut points: Vec<Point> = Vec::new();
        let mut texts: Vec<Rect> = Vec::new();
        for e in &self.edges {
            points.extend(e.points.iter().copied());
            texts.extend(e.label.iter().map(TextLines::rect));
            texts.extend(e.ends.iter().flatten().map(TextLines::rect));
        }
        for l in &self.lifelines {
            points.push(Point { x: l.x, y: l.top });
            points.push(Point {
                x: l.x,
                y: l.bottom,
            });
        }
        for m in &self.messages {
            points.push(Point {
                x: m.from_x,
                y: m.y,
            });
            points.push(Point { x: m.to_x, y: m.y });
        }
        if let Some(d) = self.start_dot {
            points.push(Point {
                x: d.x - DOT_RADIUS,
                y: d.y - DOT_RADIUS,
            });
            points.push(Point {
                x: d.x + DOT_RADIUS,
                y: d.y + DOT_RADIUS,
            });
        }
        for t in texts {
            rect(t);
        }
        for p in points {
            grow(p.x, p.y, p.x, p.y);
        }
        acc.map(|(x0, y0, x1, y1)| Rect {
            x: x0,
            y: y0,
            w: x1 - x0,
            h: y1 - y0,
        })
    }

    /// Moves everything by `(dx, dy)`.
    pub(crate) fn translate(&mut self, dx: f32, dy: f32) {
        let shift = |p: &mut Point| {
            p.x += dx;
            p.y += dy;
        };
        let shift_text = |t: &mut TextLines| {
            t.at.x += dx;
            t.at.y += dy;
        };
        let shift_rect = |r: &mut Rect| {
            r.x += dx;
            r.y += dy;
        };
        for b in &mut self.boxes {
            shift_rect(&mut b.rect);
            shift_text(&mut b.title);
            if let Some(s) = &mut b.sub {
                shift_text(s);
            }
            for r in &mut b.rows {
                shift_rect(&mut r.rect);
                shift_text(&mut r.text);
            }
        }
        for e in &mut self.edges {
            e.points.iter_mut().for_each(shift);
            if let Some(l) = &mut e.label {
                shift_text(l);
            }
            for t in e.ends.iter_mut().flatten() {
                shift_text(t);
            }
        }
        for g in &mut self.groups {
            shift_rect(&mut g.rect);
            shift_text(&mut g.label);
        }
        for l in &mut self.lifelines {
            l.x += dx;
            l.top += dy;
            l.bottom += dy;
        }
        for a in &mut self.activations {
            shift_rect(&mut a.rect);
        }
        for m in &mut self.messages {
            m.y += dy;
            m.from_x += dx;
            m.to_x += dx;
            shift_text(&mut m.label);
        }
        if let Some(d) = &mut self.start_dot {
            shift(d);
        }
    }

    /// Moves the drawing so its top left is at `(margin, margin)` and sets `size` to the
    /// drawing plus the far margin.
    pub(crate) fn normalise(&mut self, margin: f32) {
        match self.bounds() {
            Some(b) => {
                self.translate(margin - b.x, margin - b.y);
                self.size = (b.w + 2.0 * margin, b.h + 2.0 * margin);
            }
            None => self.size = (2.0 * margin, 2.0 * margin),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(x: f32, y: f32) -> TextLines {
        TextLines {
            lines: vec!["a".into()],
            style: TextStyle::Title,
            at: Point { x, y },
            width: 10.0,
            height: 16.0,
            align: Align::Left,
        }
    }

    #[test]
    fn rects_touching_do_not_intersect() {
        let a = Rect {
            x: 0.0,
            y: 0.0,
            w: 10.0,
            h: 10.0,
        };
        let b = Rect {
            x: 10.0,
            y: 0.0,
            w: 10.0,
            h: 10.0,
        };
        let c = Rect {
            x: 9.0,
            y: 9.0,
            w: 10.0,
            h: 10.0,
        };
        assert!(!a.intersects(&b));
        assert!(a.intersects(&c));
        assert!(!a.inflate(-1.0).intersects(&b.inflate(-1.0)));
    }

    #[test]
    fn normalise_moves_the_drawing_to_the_margin() {
        let mut g = Geometry::empty();
        g.edges.push(EdgeGeo {
            index: 0,
            points: vec![Point { x: -30.0, y: 5.0 }, Point { x: 70.0, y: 5.0 }],
            arrow: Arrow::End,
            label: Some(text(-20.0, -40.0)),
            ends: [None, None],
            style: LineStyle::Solid,
            change: Change::Unchanged,
            history: None,
        });
        g.normalise(24.0);
        let b = g.bounds().expect("bounds");
        assert_eq!((b.x, b.y), (24.0, 24.0));
        assert_eq!(g.size, (b.w + 48.0, b.h + 48.0));
        assert_eq!(
            g.edges[0].points[0],
            Point {
                x: 24.0,
                y: 24.0 + 40.0 + 5.0
            }
        );
    }

    #[test]
    fn an_empty_drawing_is_two_margins() {
        let mut g = Geometry::empty();
        g.normalise(24.0);
        assert_eq!(g.size, (48.0, 48.0));
    }
}
