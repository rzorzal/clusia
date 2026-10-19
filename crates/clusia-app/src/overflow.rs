//! Text must stay inside its box: a layout of every demo scene, with long data, finds any
//! `Text` that runs past an ancestor that cannot scroll it.

use bevy::prelude::*;
use clap::ValueEnum;

use crate::args::Scene;
use crate::fixture;
use crate::scenes::{stage, stage_snapshot};
use crate::testing::{self, NOW};

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

/// No scene lets the text of long data run out of its box: a long title, path, body or
/// branch name wraps or shrinks inside the screen it is on, at the default and the largest
/// text size, in a window of 1280 × 800 and in a short one.
#[test]
fn long_data_stays_inside_every_scene() {
    let mut escaped = Vec::new();
    for size in [UVec2::new(1280, 800), UVec2::new(1280, 420)] {
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
                lengthen(&mut app);
                lengthen_chat(&mut app, *scene);
                testing::settle(&mut app);
                testing::settle(&mut app);
                for line in testing::overflowing_text(&mut app) {
                    if line.contains("refresh_with_retry") {
                        escaped.push(format!("{size} large={large} {scene:?}: {line}"));
                    }
                }
            }
        }
    }
    assert_eq!(escaped, Vec::<String>::new());
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

/// Every region that can hold more than the window shows scrolls: no row of a crowded
/// scene is cut at the bottom of a box that cannot scroll.
#[test]
fn crowded_scenes_scroll_instead_of_cutting_rows() {
    let mut cut = Vec::new();
    for size in [UVec2::new(1280, 800), UVec2::new(1280, 420)] {
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
                crowd(&mut app);
                testing::settle(&mut app);
                testing::settle(&mut app);
                for line in testing::overflowing_text(&mut app) {
                    if line.contains("crowd_item_") {
                        cut.push(format!("{size} large={large} {scene:?}: {line}"));
                    }
                }
            }
        }
    }
    for c in &cut {
        eprintln!("CUT {c}");
    }
    assert_eq!(cut, Vec::<String>::new());
}
