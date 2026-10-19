//! Text must stay inside its box: a layout of every demo scene, with long data, finds any
//! `Text` that runs past an ancestor that cannot scroll it.

use bevy::prelude::*;
use clap::ValueEnum;

use crate::args::Scene;
use crate::fixture;
use crate::scenes::{stage, stage_snapshot};
use crate::testing::{self, NOW};

/// Settings whose values are long: a program path, arguments, a command and a host.
fn lengthen_config(app: &mut App) {
    let l = long();
    for (key, value) in [
        (
            "harness.program",
            format!("/opt/{TOKEN}/{TOKEN}/bin/{TOKEN}"),
        ),
        ("harness.extra_args", l.clone()),
        (
            "harness.custom_name",
            "refresh_with_retry_custom_command_name_1".to_string(),
        ),
        ("editor.custom_command", l.clone()),
        ("github.host", format!("{TOKEN}.example.dev")),
        (
            "repositories.roots",
            format!("[\"/Users/mona/{TOKEN}/{TOKEN}\"]"),
        ),
    ] {
        testing::set_config_locally(app, key, &value);
    }
}

/// A chat whose every kind of line is long, and a permission question with long details.
fn lengthen_chat(app: &mut App, scene: Scene) {
    use crate::screens::review::agent::{Chats, PanelTab, PermissionQueue};
    use clusia_protocol::{AgentErrorKind, AgentLogEntry};
    if !matches!(
        scene,
        Scene::AgentChat
            | Scene::AgentChatCodex
            | Scene::AgentChatCustom
            | Scene::Permission
            | Scene::PermissionCodex
            | Scene::PermissionOther
    ) {
        return;
    }
    let l = long();
    let pr = fixture::demo_pr();
    let log = vec![
        AgentLogEntry::User {
            at: NOW,
            turn: 1,
            text: l.clone(),
        },
        AgentLogEntry::Text {
            at: NOW,
            turn: 1,
            text: format!(
                "{l}\n\n```rust\nlet x = {l};\n```\n\n| a | b |\n|---|---|\n| {l} | {l} |"
            ),
        },
        AgentLogEntry::ToolUse {
            at: NOW,
            turn: 1,
            summary: l.clone(),
        },
        AgentLogEntry::Denied {
            at: NOW,
            turn: 1,
            tool: "Bash".into(),
            detail: l.clone(),
        },
        AgentLogEntry::Error {
            at: NOW,
            turn: 1,
            kind: AgentErrorKind::Crashed,
            message: l.clone(),
        },
    ];
    let mut chats = app.world_mut().resource_mut::<Chats>();
    let chat = chats.entry(&pr);
    chat.show(PanelTab::Agent);
    chat.log_asked = true;
    chats.replay(&pr, &log);
    let mut request = fixture::demo_permission_request(NOW * 1000);
    request.id = "perm-long".into();
    request.summary = l.clone();
    request.reason = Some(l.clone());
    request.prefix = Some(TOKEN.into());
    request.detail = Some(format!("{l}\n{l}"));
    app.world_mut()
        .resource_mut::<PermissionQueue>()
        .push(request, NOW * 1000);
}

const TOKEN: &str = "refresh_with_retry_and_a_very_long_identifier_that_has_no_space_in_it_at_all";

fn long() -> String {
    format!("{TOKEN} {TOKEN} {TOKEN}")
}

/// Replaces every string under a `path`, `body` or `title` key by `long`.
fn stretch(value: &mut serde_json::Value, long: &str) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                match (key.as_str(), &v) {
                    ("path" | "body" | "title", serde_json::Value::String(_)) => {
                        *v = serde_json::Value::String(long.to_string());
                    }
                    _ => stretch(v, long),
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(|v| stretch(v, long)),
        _ => {}
    }
}

fn lengthen(app: &mut App) {
    use crate::bridge::Model;
    use crate::review_state::{Phase, ReviewTabs};
    let l = long();
    {
        let model = &mut app.world_mut().resource_mut::<Model>().snapshot;
        for p in model.assigned.iter_mut().chain(model.mine.iter_mut()) {
            p.title = l.clone();
            p.author = TOKEN.into();
        }
        for r in &mut model.reviews {
            r.title = l.clone();
        }
    }
    let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
    for tab in tabs.0.values_mut() {
        if let Phase::Ready(ready) = &mut tab.phase {
            let v = &mut ready.view;
            v.review.title = l.clone();
            v.pr.summary.title = l.clone();
            v.pr.head_ref = TOKEN.into();
            v.pr.body =
                format!("{l}\n\n```rust\nlet x = {l};\n```\n\n| a | b |\n|---|---|\n| {l} | {l} |");
            for item in &mut v.review.draft.items {
                item.body = format!("{l}\n\n```rust\nlet x = {l};\n```");
                if let Some(a) = &mut item.anchor {
                    a.path = format!("src/{TOKEN}/{TOKEN}.rs");
                }
            }
            let mut json = serde_json::to_value(&*v).expect("a view is JSON");
            stretch(&mut json, &l);
            *v = serde_json::from_value(json).expect("still a view");
            for n in &mut ready.news {
                n.summary = l.clone();
            }
        }
    }
}

/// The pickers are placed by the window, which the test app lacks, so their cards are laid out
/// at the window's corner without the room the app gives them.
const UNPLACED: [Scene; 2] = [Scene::Emoji, Scene::Gif];

/// Text that leaves its box in the demo on purpose or because of how the test app lays out,
/// each with the reason. Nothing else may leave its box.
fn by_design(scene: Scene, line: &str) -> Option<&'static str> {
    if UNPLACED.contains(&scene) && line.contains(" below by ") {
        return Some("the bottom of a picker card: the app caps its height by the window");
    }
    None
}

/// Lays out every demo scene at `size`, the default and Large text size, stages it with
/// `prepare`, and returns every text or scroll box that leaves its box.
fn escapes(sizes: &[UVec2], prepare: impl Fn(&mut App, Scene)) -> Vec<String> {
    let mut found = Vec::new();
    for &size in sizes {
        for large in [false, true] {
            for scene in Scene::value_variants() {
                if scene.is_keywords() {
                    continue;
                }
                let mut app = testing::layout_app(fixture::demo(NOW), size);
                if large {
                    testing::set_config_locally(&mut app, "appearance.text_size", "large");
                }
                stage_snapshot(app.world_mut(), *scene);
                stage(app.world_mut(), *scene);
                testing::settle(&mut app);
                prepare(&mut app, *scene);
                testing::settle(&mut app);
                for line in testing::overflowing_text(&mut app) {
                    if by_design(*scene, &line).is_none() {
                        found.push(format!("{size} large={large} {scene:?}: {line}"));
                    }
                }
            }
        }
    }
    found
}

/// A scroll box shelters what is in it only while it fits in what holds it: one taller than its
/// window cannot be scrolled to its end.
#[test]
fn a_scroll_box_bigger_than_its_parent_is_found() {
    use crate::fonts::UiFonts;
    use crate::ui::kit::text;
    let fits = |height: f32| {
        let mut app = testing::layout_app(fixture::demo(NOW), UVec2::new(400, 300));
        let fonts = app.world().resource::<UiFonts>().clone();
        app.world_mut()
            .spawn(Node {
                position_type: PositionType::Absolute,
                width: px(200),
                height: px(100),
                ..default()
            })
            .with_children(|window| {
                window
                    .spawn(Node {
                        width: percent(100),
                        height: px(height),
                        flex_shrink: 0.0,
                        overflow: Overflow::scroll_y(),
                        ..default()
                    })
                    .with_children(|scroller| {
                        scroller.spawn(text(&fonts, "inside", crate::ui::kit::Type::BODY));
                    });
            });
        testing::settle(&mut app);
        testing::overflowing_text(&mut app)
            .into_iter()
            .filter(|l| l.starts_with("scroll box") || l.contains("inside"))
            .collect::<Vec<_>>()
    };
    assert_eq!(fits(100.0), Vec::<String>::new(), "a scroll box that fits");
    let found = fits(500.0);
    assert!(
        found
            .iter()
            .any(|l| l.starts_with("scroll box") && l.contains("below by")),
        "{found:?}"
    );
}

/// A text that breaks only between words, in a box of fixed width, runs past it when one word
/// is wider than the box; a text that also breaks inside a word does not.
#[test]
fn a_long_word_in_a_fixed_width_box_is_found_unless_the_text_breaks_inside_words() {
    use crate::fonts::UiFonts;
    use crate::ui::kit::{Type, text, wrapped_text};
    let word = "refresh_with_retry_and_a_very_long_identifier_with_no_space";
    let mut app = testing::layout_app(fixture::demo(NOW), UVec2::new(600, 300));
    let fonts = app.world().resource::<UiFonts>().clone();
    for (left, breaks_inside) in [(0.0, false), (300.0, true)] {
        app.world_mut()
            .spawn(Node {
                position_type: PositionType::Absolute,
                left: px(left),
                width: px(100),
                height: px(200),
                flex_direction: FlexDirection::Column,
                ..default()
            })
            .with_children(|cell| {
                if breaks_inside {
                    cell.spawn(wrapped_text(&fonts, word, Type::BODY));
                } else {
                    cell.spawn(text(&fonts, word, Type::BODY));
                }
            });
    }
    testing::settle(&mut app);
    let found: Vec<String> = testing::overflowing_text(&mut app)
        .into_iter()
        .filter(|l| l.contains("refresh_with_retry"))
        .collect();
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("right by"), "{found:?}");
}

/// A word wider than its box by a few pixels is found: a line's trailing space is not counted
/// as part of the text.
#[test]
fn a_word_a_few_pixels_wider_than_its_box_is_found() {
    use crate::fonts::UiFonts;
    use crate::ui::kit::{Type, text};
    let mut app = testing::layout_app(fixture::demo(NOW), UVec2::new(600, 300));
    let fonts = app.world().resource::<UiFonts>().clone();
    app.world_mut()
        .spawn(Node {
            position_type: PositionType::Absolute,
            top: px(200),
            width: px(400),
            flex_direction: FlexDirection::Column,
            ..default()
        })
        .with_children(|c| {
            c.spawn(text(&fonts, "overrunningword", Type::BODY));
        });
    testing::settle(&mut app);
    let natural = {
        let mut q = app.world_mut().query::<(&Text, &ComputedNode)>();
        let (_, n) = q
            .iter(app.world())
            .find(|(t, _)| t.0 == "overrunningword")
            .expect("laid out");
        n.content_size.x * n.inverse_scale_factor
    };
    let found_with = |width: f32| {
        let mut app = testing::layout_app(fixture::demo(NOW), UVec2::new(600, 300));
        let fonts = app.world().resource::<UiFonts>().clone();
        app.world_mut()
            .spawn(Node {
                position_type: PositionType::Absolute,
                top: px(200),
                width: px(width),
                flex_direction: FlexDirection::Column,
                ..default()
            })
            .with_children(|c| {
                c.spawn(text(&fonts, "overrunningword", Type::BODY));
            });
        testing::settle(&mut app);
        testing::overflowing_text(&mut app)
            .into_iter()
            .filter(|l| l.contains("overrunningword"))
            .count()
    };
    assert_eq!(found_with(natural.ceil()), 0, "a box as wide as the word");
    assert_eq!(found_with(natural - 8.0), 1, "a box 8 px short of it");
}

const SIZES: [UVec2; 2] = [UVec2::new(1280, 800), UVec2::new(960, 640)];

/// The demo itself, at the default and the largest text, in the window the app opens at and in
/// its smallest: nothing leaves its box, and every scroll box fits in what holds it.
#[test]
fn the_demo_stays_inside_every_scene() {
    assert_eq!(escapes(&SIZES, |_, _| {}), Vec::<String>::new());
}

/// Long titles, paths, bodies, branch names, chat lines, a permission question and settings
/// values wrap or shrink inside the screen they are on.
#[test]
fn long_data_stays_inside_every_scene() {
    let found = escapes(&SIZES, |app, scene| {
        lengthen(app);
        lengthen_chat(app, scene);
        lengthen_config(app);
    });
    assert_eq!(found, Vec::<String>::new());
}

/// Many more rows than a window holds: every region that holds them scrolls.
#[test]
fn crowded_scenes_scroll_instead_of_cutting_rows() {
    assert_eq!(escapes(&SIZES, |app, _| crowd(app)), Vec::<String>::new());
}

/// Many more rows than a window holds: news, draft comments and findings.
fn crowd(app: &mut App) {
    use crate::bridge::{ChecksTell, Tell};
    use crate::review_state::{Phase, ReviewTabs};
    let pr = fixture::demo_pr();
    {
        let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
        for tab in tabs.0.values_mut() {
            if let Phase::Ready(ready) = &mut tab.phase {
                let template = ready.news.first().cloned();
                if let Some(template) = template {
                    for n in 0..30 {
                        let mut item = template.clone();
                        item.summary = format!("crowd_item_{n}");
                        ready.news.push(item);
                    }
                }
                let draft = &mut ready.view.review.draft;
                let first = draft.items.first().cloned();
                if let Some(first) = first {
                    for n in 0..20 {
                        let mut item = first.clone();
                        item.id = format!("crowd{n}");
                        item.body = format!("crowd_item_{n}");
                        draft.items.push(item);
                    }
                }
            }
        }
    }
    let (mut results, states) = fixture::demo_checks(NOW);
    for result in &mut results {
        let first = result.findings.first().cloned();
        if let Some(first) = first {
            for n in 0..12 {
                let mut f = first.clone();
                f.id = format!("crowd{n}");
                f.title = format!("crowd_item_{n}");
                result.findings.push(f);
            }
        }
    }
    testing::tell(
        app,
        Tell::Checks(ChecksTell::Loaded {
            pr,
            results,
            states,
            accepted: Vec::new(),
            dismissed: Vec::new(),
        }),
    );
}
