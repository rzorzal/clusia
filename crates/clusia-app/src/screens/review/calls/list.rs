//! The Calls view's left column: the find box above the functions of the pull request, grouped
//! by how they changed, or the find box's hits while it holds text. A row re-centres the
//! graph on its function, or selects a file the call graph cannot read.

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::picking::Pickable;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::text::EditableText;
use bevy::ui_widgets::{Activate, Button as WidgetButton, ScrollArea, observe};
use clusia_calls::{ChangeKind, NodeKind, language_name};
use clusia_core::PrRef;
use clusia_protocol::FunctionHit;

use super::model::{Calls, CallsModel};
use crate::fonts::UiFonts;
use crate::review_state::{ReviewTabs, TabUi};
use crate::theme::Swatch;
use crate::ui::kit::{self, Clickable, Fill, HoverFill, Type, badge, text, text_field};

/// The find box of `pr`'s Calls view.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct FindField(pub PrRef);

/// *Find any function*, over the find box while it is empty.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct FindHint(pub PrRef);

/// A row of the left column: the function's id, or the path of a file with no call graph.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct LeftRow(pub PrRef, pub String);

/// A `LeftRow` that stands for a file, not a function.
#[derive(Component, Debug, Clone, Copy)]
pub struct LeftFile;

/// The column's find box, filled once.
#[derive(Component)]
pub struct FindPart {
    pub pr: PrRef,
    filled: bool,
}

/// The groups under the find box; rebuilt when its `LeftView` changes.
#[derive(Component)]
pub struct LeftPart {
    pub pr: PrRef,
    built: Option<LeftView>,
}

/// The colour of a row's tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Changed,
    Signature,
    Moved,
    New,
    Plain,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub key: String,
    pub label: String,
    pub tag: Option<String>,
    pub tone: Tone,
    /// `path:line`, under a hit of the find box.
    pub detail: Option<String>,
    pub selected: bool,
    /// A file with no call graph, not a function.
    pub file: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub title: String,
    pub rows: Vec<Row>,
    /// Functions the group counts but does not list.
    pub more: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeftView {
    pub groups: Vec<Group>,
    /// The hits of the find box, in place of the groups, while it holds text.
    pub results: Option<Vec<Row>>,
}

/// The tag and tone of a function the pull request changes.
pub(crate) fn change_tag(kind: ChangeKind) -> (&'static str, Tone) {
    match kind {
        ChangeKind::Changed | ChangeKind::Removed => ("changed", Tone::Changed),
        ChangeKind::Signature => ("signature", Tone::Signature),
        ChangeKind::Moved => ("moved", Tone::Moved),
        ChangeKind::New => ("new", Tone::New),
    }
}

/// The tag and tone of a node of the graph; an untouched, outside or *+N* node has none.
pub(crate) fn node_tag(kind: NodeKind) -> Option<(&'static str, Tone)> {
    match kind {
        NodeKind::Changed => Some(("changed", Tone::Changed)),
        NodeKind::Signature => Some(("signature", Tone::Signature)),
        NodeKind::Moved => Some(("moved", Tone::Moved)),
        NodeKind::New => Some(("new", Tone::New)),
        NodeKind::Untouched | NodeKind::Outside | NodeKind::More => None,
    }
}

/// The language of a file, as its row's tag.
pub fn file_tag(path: &str) -> Option<String> {
    language_name(path).map(String::from)
}

/// The file the canvas and the panel show when the graph cannot be drawn: the selected one, else
/// the first.
pub fn selected_file(ui: &TabUi, unsupported: &[String]) -> Option<String> {
    ui.calls_node
        .as_ref()
        .filter(|n| unsupported.contains(n))
        .or_else(|| unsupported.first())
        .cloned()
}

fn hit_row(hit: &FunctionHit, centre: Option<&str>, detail: bool) -> Row {
    let tag = match (hit.changed, hit.kind) {
        (true, Some(kind)) => Some(change_tag(kind)),
        (true, None) => Some(("changed", Tone::Changed)),
        (false, _) => None,
    };
    Row {
        key: hit.id.clone(),
        label: hit.qualified.clone(),
        tag: tag.map(|(t, _)| t.to_string()),
        tone: tag.map_or(Tone::Plain, |(_, tone)| tone),
        detail: detail.then(|| format!("{}:{}", hit.path, hit.line)),
        selected: centre == Some(hit.id.as_str()),
        file: false,
    }
}

fn node_row(node: &clusia_calls::GraphNode, centre: Option<&str>) -> Row {
    let tag = node_tag(node.kind);
    Row {
        key: node.key.clone(),
        label: node.label.clone(),
        tag: tag.map(|(t, _)| t.to_string()),
        tone: tag.map_or(Tone::Plain, |(_, tone)| tone),
        detail: None,
        selected: centre == Some(node.key.as_str()),
        file: false,
    }
}

fn group(title: String, rows: Vec<Row>, more: u32) -> Option<Group> {
    (!rows.is_empty() || more > 0).then_some(Group { title, rows, more })
}

/// The left column of `model` for the tab `ui`. The changed functions come from the empty
/// search (the daemon's order: path, then line), else from the graph. A group with nothing in
/// it is left out.
pub fn left_view(model: &CallsModel, ui: &TabUi) -> LeftView {
    let graph = model.graph.as_ref();
    let centre = graph.map(|g| ui.calls_center.as_deref().unwrap_or(g.center.as_str()));
    let results = (!model.find.is_empty()).then(|| {
        model
            .hits
            .iter()
            .map(|h| hit_row(h, centre, true))
            .collect()
    });
    let (changed, added): (Vec<Row>, Vec<Row>) = if model.hits.is_empty() || !model.find.is_empty()
    {
        let nodes = graph.map_or(&[][..], |g| g.nodes.as_slice());
        let of = |keep: fn(NodeKind) -> bool| -> Vec<Row> {
            nodes
                .iter()
                .filter(|n| keep(n.kind))
                .map(|n| node_row(n, centre))
                .collect()
        };
        (
            of(|k| matches!(k, NodeKind::Changed | NodeKind::Signature | NodeKind::Moved)),
            of(|k| k == NodeKind::New),
        )
    } else {
        let changed = model.hits.iter().filter(|h| h.changed);
        (
            changed
                .clone()
                .filter(|h| h.kind != Some(ChangeKind::New))
                .map(|h| hit_row(h, centre, false))
                .collect(),
            changed
                .filter(|h| h.kind == Some(ChangeKind::New))
                .map(|h| hit_row(h, centre, false))
                .collect(),
        )
    };
    let mut groups = Vec::new();
    groups.extend(group(
        format!("Changed in this PR · {}", changed.len()),
        changed,
        0,
    ));
    groups.extend(group(format!("Added · {}", added.len()), added, 0));
    if let Some(g) = graph {
        let mut called: Vec<&clusia_calls::GraphNode> = g
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Untouched && n.level != 0)
            .collect();
        called.sort_by(|a, b| (a.level, &a.label).cmp(&(b.level, &b.label)));
        let rows: Vec<Row> = called.into_iter().map(|n| node_row(n, centre)).collect();
        let more = g.touched.saturating_sub(rows.len() as u32);
        groups.extend(group(
            format!("Called, not changed · {}", g.touched),
            rows,
            more,
        ));
    }
    if !model.unsupported.is_empty() {
        let shown = selected_file(ui, &model.unsupported);
        let n = model.unsupported.len();
        groups.push(Group {
            title: format!(
                "No call graph yet · {n} {}",
                if n == 1 { "file" } else { "files" }
            ),
            rows: model
                .unsupported
                .iter()
                .map(|path| Row {
                    key: path.clone(),
                    label: path.clone(),
                    tag: file_tag(path),
                    tone: Tone::Plain,
                    detail: None,
                    selected: shown.as_deref() == Some(path.as_str()),
                    file: true,
                })
                .collect(),
            more: 0,
        });
    }
    LeftView { groups, results }
}

// ---- the column

/// The width of the find box: the column's 258 px less its padding.
const FIELD_WIDTH: f32 = 226.0;

/// The left column of the Calls frame.
pub(super) fn column(p: &mut ChildSpawnerCommands, pr: &PrRef) {
    p.spawn((
        Node {
            width: px(258),
            flex_shrink: 0.0,
            min_height: px(0),
            flex_direction: FlexDirection::Column,
            border: UiRect::right(px(1)),
            ..default()
        },
        BackgroundColor::default(),
        Fill(Swatch::Bg),
        BorderColor::default(),
        kit::Stroke(Swatch::Line),
    ))
    .with_children(|c| {
        c.spawn((
            Node {
                padding: UiRect::new(px(16), px(16), px(12), px(4)),
                flex_shrink: 0.0,
                ..default()
            },
            FindPart {
                pr: pr.clone(),
                filled: false,
            },
        ));
        c.spawn((
            Node {
                flex_grow: 1.0,
                min_height: px(0),
                flex_direction: FlexDirection::Column,
                padding: UiRect::axes(px(10), px(8)),
                overflow: Overflow {
                    x: OverflowAxis::Clip,
                    y: OverflowAxis::Scroll,
                },
                ..default()
            },
            ScrollArea,
            ScrollPosition::default(),
            LeftPart {
                pr: pr.clone(),
                built: None,
            },
        ));
    });
}

/// Fills each new find box with what the model searches for, and rebuilds each list whose view
/// changed.
pub(super) fn fill_left(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    calls: Res<Calls>,
    fonts: Res<UiFonts>,
    mut finds: Query<(Entity, &mut FindPart)>,
    mut lists: Query<(Entity, &mut LeftPart)>,
) {
    for (entity, mut part) in &mut finds {
        if part.filled {
            continue;
        }
        part.filled = true;
        let value = calls
            .0
            .get(&part.pr)
            .map(|m| m.find.clone())
            .unwrap_or_default();
        let pr = part.pr.clone();
        commands.entity(entity).with_children(|f| {
            f.spawn(Node {
                width: px(FIELD_WIDTH),
                ..default()
            })
            .with_children(|w| {
                w.spawn((
                    text_field(&fonts, &value, FIELD_WIDTH, false),
                    FindField(pr.clone()),
                ));
                w.spawn((
                    Node {
                        position_type: PositionType::Absolute,
                        left: px(11),
                        top: px(6),
                        ..default()
                    },
                    text(&fonts, "Find any function", Type::MUTED.ink(Swatch::Faint)),
                    if value.is_empty() {
                        Visibility::Inherited
                    } else {
                        Visibility::Hidden
                    },
                    Pickable::IGNORE,
                    FindHint(pr),
                ));
            });
        });
    }
    let empty = CallsModel::default();
    for (entity, mut part) in &mut lists {
        let Some(tab) = tabs.0.get(&part.pr) else {
            continue;
        };
        let model = calls.0.get(&part.pr).unwrap_or(&empty);
        let view = left_view(model, &tab.ui);
        if part.built.as_ref() == Some(&view) {
            continue;
        }
        let pr = part.pr.clone();
        commands.entity(entity).despawn_related::<Children>();
        commands
            .entity(entity)
            .with_children(|p| list_content(p, &fonts, &pr, &view));
        part.built = Some(view);
    }
}

/// The hint over the find box shows while the box is empty.
pub(super) fn sync_hint(
    fields: Query<(&FindField, &EditableText), Changed<EditableText>>,
    mut hints: Query<(&FindHint, &mut Visibility)>,
) {
    for (field, editable) in &fields {
        let want = if editable.value().to_string().is_empty() {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
        for (hint, mut visibility) in &mut hints {
            if hint.0 == field.0 && *visibility != want {
                *visibility = want;
            }
        }
    }
}

fn list_content(p: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, v: &LeftView) {
    if let Some(results) = &v.results {
        if results.is_empty() {
            p.spawn((
                Node {
                    padding: UiRect::axes(px(8), px(10)),
                    ..default()
                },
                children![text(fonts, "No function by that name", Type::MUTED)],
            ));
        }
        for row in results {
            row_button(p, fonts, pr, row);
        }
        return;
    }
    for g in &v.groups {
        p.spawn(Node {
            padding: UiRect::new(px(8), px(8), px(14), px(6)),
            ..default()
        })
        .with_children(|t| {
            t.spawn(text(fonts, g.title.to_uppercase(), Type::META));
        });
        for row in &g.rows {
            row_button(p, fonts, pr, row);
        }
        if g.more > 0 {
            p.spawn(Node {
                padding: UiRect::axes(px(8), px(6)),
                ..default()
            })
            .with_children(|m| {
                m.spawn(text(
                    fonts,
                    format!("+{} more", g.more),
                    Type::MUTED.ink(Swatch::Faint),
                ));
            });
        }
    }
}

fn tone_of(tone: Tone) -> kit::Tone {
    match tone {
        Tone::Changed | Tone::New => kit::Tone::Green,
        Tone::Signature => kit::Tone::Orange,
        Tone::Moved | Tone::Plain => kit::Tone::Neutral,
    }
}

fn row_button(p: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, row: &Row) {
    let mut e = p.spawn((
        Node {
            flex_direction: FlexDirection::Column,
            row_gap: px(2),
            padding: UiRect::axes(px(8), px(6)),
            border_radius: BorderRadius::all(px(6)),
            flex_shrink: 0.0,
            ..default()
        },
        WidgetButton,
        Clickable,
        Hovered::default(),
        TabIndex(0),
        BackgroundColor::default(),
        Fill(if row.selected {
            Swatch::Selected
        } else {
            Swatch::Clear
        }),
        HoverFill(if row.selected {
            Swatch::Selected
        } else {
            Swatch::Hover
        }),
        LeftRow(pr.clone(), row.key.clone()),
        observe(on_row),
    ));
    if row.file {
        e.insert(LeftFile);
    }
    e.with_children(|r| {
        r.spawn((
            Node {
                column_gap: px(8),
                align_items: AlignItems::Center,
                ..default()
            },
            Pickable::IGNORE,
        ))
        .with_children(|line| {
            let weight = if row.selected { 600 } else { 400 };
            line.spawn((
                Node {
                    flex_grow: 1.0,
                    min_width: px(0),
                    overflow: Overflow::clip(),
                    ..default()
                },
                Pickable::IGNORE,
            ))
            .with_children(|l| {
                l.spawn((
                    text(
                        fonts,
                        row.label.clone(),
                        Type {
                            size: 13.0,
                            weight,
                            ink: Swatch::Fg,
                            mono: true,
                        },
                    ),
                    TextLayout::no_wrap(),
                    Pickable::IGNORE,
                ));
            });
            if let Some(tag) = &row.tag {
                line.spawn((badge(fonts, tag, tone_of(row.tone)), Pickable::IGNORE));
            }
        });
        if let Some(detail) = &row.detail {
            r.spawn((
                text(fonts, detail.clone(), Type::MONO.ink(Swatch::Faint)),
                Pickable::IGNORE,
            ));
        }
    });
}

/// A function row re-centres the graph on it; a file row selects the file.
fn on_row(
    activate: On<Activate>,
    rows: Query<(&LeftRow, Has<LeftFile>)>,
    mut tabs: ResMut<ReviewTabs>,
) {
    let Ok((LeftRow(pr, key), file)) = rows.get(activate.entity) else {
        return;
    };
    let Some(tab) = tabs.0.get_mut(pr) else {
        return;
    };
    let ui = &mut tab.ui;
    if file {
        ui.calls_node = Some(key.clone());
    } else {
        ui.calls_center = Some(key.clone());
        ui.calls_node = None;
        ui.calls_expand.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screens::review::calls::testkit::*;
    use clusia_calls::ChangeKind;
    use clusia_protocol::FunctionHit;

    fn hit(key: &str, qualified: &str, kind: Option<ChangeKind>) -> FunctionHit {
        let mut parts = key.splitn(3, '#');
        FunctionHit {
            id: key.into(),
            qualified: qualified.into(),
            path: parts.next().unwrap().into(),
            line: parts.nth(1).unwrap().parse().unwrap(),
            changed: kind.is_some(),
            kind,
        }
    }

    fn model() -> CallsModel {
        CallsModel {
            graph: Some(sample()),
            // What an empty `FindFunction` answers: path, then line.
            hits: vec![
                hit(REFRESH, "TokenStore::refresh", Some(ChangeKind::Changed)),
                hit(NEW, "TokenStore::new", Some(ChangeKind::Signature)),
                hit(SAVE, "TokenStore::save", Some(ChangeKind::Moved)),
                hit(EXCHANGE, "HttpClient::exchange", Some(ChangeKind::Changed)),
                hit(RACE, "tests::refresh_race", Some(ChangeKind::New)),
            ],
            ..CallsModel::default()
        }
    }

    fn rows(g: &Group) -> Vec<(&str, Option<&str>)> {
        g.rows
            .iter()
            .map(|r| (r.label.as_str(), r.tag.as_deref()))
            .collect()
    }

    #[test]
    fn the_list_has_the_changed_the_added_and_the_called_groups() {
        let v = left_view(&model(), &TabUi::default());
        let titles: Vec<&str> = v.groups.iter().map(|g| g.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "Changed in this PR · 4",
                "Added · 1",
                "Called, not changed · 21"
            ]
        );
        assert_eq!(
            rows(&v.groups[0]),
            [
                ("TokenStore::refresh", Some("changed")),
                ("TokenStore::new", Some("signature")),
                ("TokenStore::save", Some("moved")),
                ("HttpClient::exchange", Some("changed")),
            ],
            "the daemon's order, tagged by the hit's own kind"
        );
        assert_eq!(rows(&v.groups[1]), [("tests::refresh_race", Some("new"))]);
        assert_eq!(
            rows(&v.groups[2]),
            [("auth_middleware", None), ("TokenStore::token", None)]
        );
        assert_eq!(v.groups[2].more, 19, "21 touched, 2 listed");
        assert!(v.results.is_none());
    }

    #[test]
    fn the_centre_is_the_selected_row_unless_the_tab_chose_another() {
        let selected = |ui: &TabUi| -> Vec<String> {
            left_view(&model(), ui)
                .groups
                .iter()
                .flat_map(|g| g.rows.iter())
                .filter(|r| r.selected)
                .map(|r| r.label.clone())
                .collect()
        };
        assert_eq!(selected(&TabUi::default()), ["TokenStore::refresh"]);
        let ui = TabUi {
            calls_center: Some(TOKEN.into()),
            ..TabUi::default()
        };
        assert_eq!(selected(&ui), ["TokenStore::token"]);
    }

    #[test]
    fn a_hit_without_a_kind_reads_changed_and_a_long_list_is_not_cut() {
        let mut m = model();
        m.hits = (1..=60)
            .map(|n| FunctionHit {
                id: format!("src/gen.rs#f{n}#{n}"),
                qualified: format!("f{n}"),
                path: "src/gen.rs".into(),
                line: n,
                changed: true,
                kind: None,
            })
            .collect();
        let v = left_view(&m, &TabUi::default());
        assert_eq!(v.groups[0].title, "Changed in this PR · 60");
        assert_eq!(v.groups[0].rows.len(), 60);
        assert!(
            v.groups[0]
                .rows
                .iter()
                .all(|r| r.tag.as_deref() == Some("changed"))
        );
    }

    #[test]
    fn without_hits_the_changed_group_comes_from_the_graph() {
        let m = CallsModel {
            graph: Some(sample()),
            ..CallsModel::default()
        };
        let v = left_view(&m, &TabUi::default());
        assert_eq!(
            rows(&v.groups[0]),
            [
                ("TokenStore::new", Some("signature")),
                ("TokenStore::refresh", Some("changed")),
                ("HttpClient::exchange", Some("changed")),
                ("TokenStore::save", Some("moved")),
            ]
        );
    }

    #[test]
    fn files_with_no_pack_are_their_own_group_with_their_language() {
        let m = CallsModel {
            unsupported: vec!["server/handler.go".into(), "server/limits.go".into()],
            hits: model().hits,
            ..CallsModel::default()
        };
        let v = left_view(&m, &TabUi::default());
        let last = v.groups.last().unwrap();
        assert_eq!(last.title, "No call graph yet · 2 files");
        assert_eq!(
            rows(last),
            [
                ("server/handler.go", Some("Go")),
                ("server/limits.go", Some("Go"))
            ]
        );
        assert!(last.rows.iter().all(|r| r.file));
        assert!(
            last.rows[0].selected && !last.rows[1].selected,
            "the first file is selected by default"
        );
        let ui = TabUi {
            calls_node: Some("server/limits.go".into()),
            ..TabUi::default()
        };
        let v = left_view(&m, &ui);
        assert!(v.groups.last().unwrap().rows[1].selected);
        assert!(
            v.groups[0].rows.iter().all(|r| !r.selected),
            "with no graph there is no centre row"
        );
        let one = CallsModel {
            unsupported: vec!["a.go".into()],
            ..CallsModel::default()
        };
        assert_eq!(
            left_view(&one, &TabUi::default()).groups[0].title,
            "No call graph yet · 1 file"
        );
    }

    #[test]
    fn typing_shows_the_hits_with_their_places_instead_of_the_groups() {
        let mut m = model();
        m.find = "refr".into();
        let v = left_view(&m, &TabUi::default());
        let results = v.results.expect("results");
        assert_eq!(results.len(), 5);
        assert_eq!(results[0].label, "TokenStore::refresh");
        assert_eq!(results[0].detail.as_deref(), Some("src/auth/refresh.rs:38"));
        assert_eq!(results[0].key, REFRESH);
        assert_eq!(results[0].tag.as_deref(), Some("changed"));
        assert_eq!(results[1].tag.as_deref(), Some("signature"));
    }

    #[test]
    fn a_language_has_a_name_or_no_tag() {
        assert_eq!(file_tag("server/handler.go").as_deref(), Some("Go"));
        assert_eq!(file_tag("README.xyz"), None);
    }
}
