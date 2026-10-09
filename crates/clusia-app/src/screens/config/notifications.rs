//! Config › Notifications: which events reach you where, the sound, quiet hours, and the
//! macOS permission.

use std::collections::{BTreeSet, HashMap};

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::ui_widgets::{Activate, Button as WidgetButton, observe};
use clusia_core::config::{EventKind, Route, SoundId, Weekday};
use clusia_protocol::PermissionStatus;

use super::{ConfigField, FieldError, OpenLink, page_header, row, sends, setter};
use crate::bridge::Ask;
use crate::fonts::UiFonts;
use crate::nav::{Nav, Screen, Section};
use crate::platform_open::NOTIFICATION_SETTINGS;
use crate::platform_sound::{PlayedSounds, preview};
use crate::snapshot::Snapshot;
use crate::theme::Swatch;
use crate::ui::kit::{
    Clickable, Fill, HoverFill, Stroke, Type, Variant, button, checkbox, disabled_checkbox,
    segment, segments, text, text_field,
};

/// How often the open page asks the daemon whether macOS allows notifications.
const PERMISSION_POLL_SECS: f64 = 4.0;

/// The columns of the event table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    Tray,
    Macos,
    Sound,
}

impl Column {
    const ALL: [Column; 3] = [Column::Tray, Column::Macos, Column::Sound];

    fn label(self) -> &'static str {
        match self {
            Column::Tray => "Tray",
            Column::Macos => "macOS",
            Column::Sound => "Sound",
        }
    }

    fn of(self, route: Route) -> bool {
        match self {
            Column::Tray => route.tray,
            Column::Macos => route.macos,
            Column::Sound => route.sound,
        }
    }
}

/// The config keys of the events you can change, one per column.
const KEYS: [(EventKind, [&str; 3]); 8] = [
    (
        EventKind::ReviewRequested,
        [
            "notifications.events.review_requested.tray",
            "notifications.events.review_requested.macos",
            "notifications.events.review_requested.sound",
        ],
    ),
    (
        EventKind::CommitsAfterReview,
        [
            "notifications.events.commits_after_review.tray",
            "notifications.events.commits_after_review.macos",
            "notifications.events.commits_after_review.sound",
        ],
    ),
    (
        EventKind::ReplyToYou,
        [
            "notifications.events.reply_to_you.tray",
            "notifications.events.reply_to_you.macos",
            "notifications.events.reply_to_you.sound",
        ],
    ),
    (
        EventKind::Mentioned,
        [
            "notifications.events.mentioned.tray",
            "notifications.events.mentioned.macos",
            "notifications.events.mentioned.sound",
        ],
    ),
    (
        EventKind::AgentFinished,
        [
            "notifications.events.agent_finished.tray",
            "notifications.events.agent_finished.macos",
            "notifications.events.agent_finished.sound",
        ],
    ),
    (
        EventKind::ChecksFailed,
        [
            "notifications.events.checks_failed.tray",
            "notifications.events.checks_failed.macos",
            "notifications.events.checks_failed.sound",
        ],
    ),
    (
        EventKind::SyncProblem,
        [
            "notifications.events.sync_problem.tray",
            "notifications.events.sync_problem.macos",
            "notifications.events.sync_problem.sound",
        ],
    ),
    (
        EventKind::StateRecovered,
        [
            "notifications.events.state_recovered.tray",
            "notifications.events.state_recovered.macos",
            "notifications.events.state_recovered.sound",
        ],
    ),
];

fn route_key(kind: EventKind, column: Column) -> Option<&'static str> {
    KEYS.iter()
        .find(|(k, _)| *k == kind)
        .map(|(_, keys)| keys[column as usize])
}

const DND_ENABLED: &str = "notifications.dnd.enabled";
const DND_FROM: &str = "notifications.dnd.from";
const DND_TO: &str = "notifications.dnd.to";
const DND_DAYS: &str = "notifications.dnd.days";
const FOLLOW_FOCUS: &str = "notifications.follow_focus";
const GROUP_BURSTS: &str = "notifications.group_bursts";
const SOUND: &str = "notifications.sound";

#[derive(Debug, Clone, PartialEq)]
pub struct EventRow {
    pub kind: EventKind,
    pub title: &'static str,
    pub hint: String,
    pub route: Route,
    /// The agent's events cannot be changed until the agent exists.
    pub live: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NotificationsView {
    pub rows: Vec<EventRow>,
    pub sound: SoundId,
    pub dnd_enabled: bool,
    pub dnd_from: String,
    pub dnd_to: String,
    pub dnd_days: BTreeSet<Weekday>,
    /// The first refusal among the DND keys, with its key.
    pub dnd_error: Option<(&'static str, String)>,
    /// The first refusal among the event keys, with its key.
    pub events_error: Option<(&'static str, String)>,
    pub sound_error: Option<String>,
    pub follow_focus: bool,
    pub follow_focus_error: Option<String>,
    pub group_bursts: bool,
    pub group_bursts_error: Option<String>,
    pub permission: PermissionStatus,
}

fn first_refusal(
    rejected: &HashMap<String, String>,
    keys: impl IntoIterator<Item = &'static str>,
) -> Option<(&'static str, String)> {
    keys.into_iter()
        .find_map(|key| rejected.get(key).map(|m| (key, m.clone())))
}

pub fn view(snap: &Snapshot, rejected: &HashMap<String, String>) -> NotificationsView {
    let n = &snap.config.notifications;
    let login = snap
        .auth
        .as_ref()
        .and_then(|a| a.login.as_deref())
        .unwrap_or("you");
    let rows = [
        (
            EventKind::ReviewRequested,
            "Review requested from you",
            "Someone added you as a reviewer".to_string(),
        ),
        (
            EventKind::CommitsAfterReview,
            "New commits on a PR you reviewed",
            "Your review may be out of date".into(),
        ),
        (
            EventKind::ReplyToYou,
            "Reply to your comment",
            "In a thread you started or joined".into(),
        ),
        (
            EventKind::Mentioned,
            "Mentioned",
            format!("@{login} in a PR or comment"),
        ),
        (
            EventKind::AgentFinished,
            "Agent finished a review",
            "Harness pre-review is ready to read".into(),
        ),
        (
            EventKind::AgentPermission,
            "Agent needs your permission",
            "Harness asks to run a tool · arrives with the agent".into(),
        ),
        (
            EventKind::ChecksFailed,
            "Checks failed on your PR",
            "CI on pull requests you opened".into(),
        ),
        (
            EventKind::SyncProblem,
            "Sync problem",
            "Token expired, offline, rate limit".into(),
        ),
        (
            EventKind::StateRecovered,
            "Recovered a damaged file",
            "A settings or state file was reset".into(),
        ),
    ]
    .into_iter()
    .map(|(kind, title, hint)| EventRow {
        kind,
        title,
        hint,
        route: n
            .events
            .get(&kind)
            .copied()
            .unwrap_or_else(|| Route::default_for(kind)),
        live: route_key(kind, Column::Tray).is_some(),
    })
    .collect();
    NotificationsView {
        rows,
        sound: n.sound,
        dnd_enabled: n.dnd.enabled,
        dnd_from: n.dnd.from.to_string(),
        dnd_to: n.dnd.to.to_string(),
        dnd_days: n.dnd.days.clone(),
        dnd_error: first_refusal(rejected, [DND_ENABLED, DND_FROM, DND_TO, DND_DAYS]),
        events_error: first_refusal(rejected, KEYS.iter().flat_map(|(_, keys)| *keys)),
        sound_error: rejected.get(SOUND).cloned(),
        follow_focus: n.follow_focus,
        follow_focus_error: rejected.get(FOLLOW_FOCUS).cloned(),
        group_bursts: n.group_bursts,
        group_bursts_error: rejected.get(GROUP_BURSTS).cloned(),
        permission: snap.notifications_permission,
    }
}

/// Plays a sound when activated.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewSound(pub SoundId);

fn on_preview(
    activate: On<Activate>,
    sounds: Query<&PreviewSound>,
    played: Option<ResMut<PlayedSounds>>,
) {
    if let Ok(PreviewSound(id)) = sounds.get(activate.entity) {
        preview(played, *id);
    }
}

/// The `notifications.dnd.days` value with `day` switched.
pub fn days_value(days: &BTreeSet<Weekday>, day: Weekday) -> String {
    let mut next = days.clone();
    if !next.remove(&day) {
        next.insert(day);
    }
    let names: Vec<&str> = next.iter().map(|d| d.as_str()).collect();
    serde_json::to_string(&names).unwrap_or_else(|_| "[]".into())
}

fn letter(day: Weekday) -> &'static str {
    match day {
        Weekday::Mon => "M",
        Weekday::Tue | Weekday::Thu => "T",
        Weekday::Wed => "W",
        Weekday::Fri => "F",
        Weekday::Sat | Weekday::Sun => "S",
    }
}

fn pill(fonts: &UiFonts, day: Weekday, on: bool) -> impl Bundle {
    (
        Node {
            width: px(28),
            height: px(28),
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            border_radius: BorderRadius::MAX,
            flex_shrink: 0.0,
            ..default()
        },
        (WidgetButton, Clickable),
        Hovered::default(),
        TabIndex(0),
        BackgroundColor::default(),
        Fill(if on { Swatch::Green } else { Swatch::Chrome }),
        HoverFill(if on {
            Swatch::GreenHover
        } else {
            Swatch::Hover
        }),
        children![text(
            fonts,
            letter(day),
            Type::STRONG.ink(if on { Swatch::OnGreen } else { Swatch::Muted }),
        )],
    )
}

fn checks(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    label: &str,
    key: &'static str,
    on: bool,
    hint: &str,
    error: Option<&str>,
) {
    row(
        p,
        fonts,
        label,
        |r| {
            r.spawn((checkbox(on), setter(key, (!on).to_string())));
        },
        hint,
        error.map(|m| (key, m)),
    );
}

pub fn build(p: &mut ChildSpawnerCommands, fonts: &UiFonts, v: &NotificationsView) {
    p.spawn(Node {
        justify_content: JustifyContent::SpaceBetween,
        align_items: AlignItems::FlexStart,
        ..default()
    })
    .with_children(|top| {
        top.spawn(Node::default()).with_children(|h| {
            page_header(
                h,
                fonts,
                "Notifications",
                "What reaches you, and where. The tray dot always shows the total.",
            );
        });
        top.spawn((
            button(fonts, "Send a test notification", Variant::Secondary),
            sends(Ask::TestNotification),
        ));
    });
    event_table(p, fonts, v);
    sound_row(p, fonts, v);
    dnd_row(p, fonts, v);
    checks(
        p,
        fonts,
        "Follow macOS Focus",
        FOLLOW_FOCUS,
        v.follow_focus,
        "Silent while a Focus is on; the tray still counts",
        v.follow_focus_error.as_deref(),
    );
    checks(
        p,
        fonts,
        "Group bursts",
        GROUP_BURSTS,
        v.group_bursts,
        "Several events within 2 minutes become one notification",
        v.group_bursts_error.as_deref(),
    );
    permission_row(p, fonts, v.permission);
}

fn event_table(p: &mut ChildSpawnerCommands, fonts: &UiFonts, v: &NotificationsView) {
    p.spawn(crate::ui::kit::card(Node {
        flex_direction: FlexDirection::Column,
        ..default()
    }))
    .with_children(|t| {
        t.spawn(Node {
            padding: UiRect::axes(px(20), px(10)),
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(|h| {
            h.spawn((
                Node {
                    flex_grow: 1.0,
                    ..default()
                },
                children![text(fonts, "Event", Type::META.ink(Swatch::Muted))],
            ));
            for column in Column::ALL {
                cell(h, |c| {
                    c.spawn(text(fonts, column.label(), Type::META.ink(Swatch::Muted)));
                });
            }
        });
        for r in &v.rows {
            t.spawn((
                Node {
                    padding: UiRect::axes(px(20), px(10)),
                    align_items: AlignItems::Center,
                    border: UiRect::top(px(1)),
                    ..default()
                },
                BorderColor::default(),
                Stroke(Swatch::Line),
            ))
            .with_children(|line| {
                line.spawn(Node {
                    flex_grow: 1.0,
                    flex_direction: FlexDirection::Column,
                    row_gap: px(2),
                    ..default()
                })
                .with_children(|names| {
                    let ink = if r.live { Swatch::Fg } else { Swatch::Faint };
                    names.spawn(text(fonts, r.title, Type::STRONG.ink(ink)));
                    names.spawn(text(fonts, r.hint.clone(), Type::META));
                });
                for column in Column::ALL {
                    let on = column.of(r.route);
                    cell(line, |c| match route_key(r.kind, column) {
                        Some(key) => {
                            c.spawn((checkbox(on), setter(key, (!on).to_string())));
                        }
                        None => {
                            c.spawn(disabled_checkbox(on));
                        }
                    });
                }
            });
        }
    });
    if let Some((key, message)) = &v.events_error {
        p.spawn((
            Node {
                margin: UiRect::top(px(4)),
                ..default()
            },
            FieldError(key),
            children![text(fonts, message.clone(), Type::BODY.ink(Swatch::Orange))],
        ));
    }
}

fn cell(p: &mut ChildSpawnerCommands, fill: impl FnOnce(&mut ChildSpawnerCommands)) {
    p.spawn(Node {
        width: px(90),
        justify_content: JustifyContent::Center,
        flex_shrink: 0.0,
        ..default()
    })
    .with_children(fill);
}

fn sound_row(p: &mut ChildSpawnerCommands, fonts: &UiFonts, v: &NotificationsView) {
    row(
        p,
        fonts,
        "Sound",
        |r| {
            r.spawn(segments()).with_children(|s| {
                for id in SoundId::ALL {
                    s.spawn((
                        segment(fonts, id.label(), id == v.sound),
                        setter(SOUND, id.as_str()),
                    ));
                }
            });
            r.spawn((
                button(fonts, "Play", Variant::Secondary),
                PreviewSound(v.sound),
                observe(on_preview),
            ));
        },
        "",
        v.sound_error.as_deref().map(|m| (SOUND, m)),
    );
}

fn dnd_row(p: &mut ChildSpawnerCommands, fonts: &UiFonts, v: &NotificationsView) {
    let error = v.dnd_error.as_ref().map(|(key, m)| (*key, m.as_str()));
    row(
        p,
        fonts,
        "Do not disturb",
        |r| {
            r.spawn((
                checkbox(v.dnd_enabled),
                setter(DND_ENABLED, (!v.dnd_enabled).to_string()),
            ));
            r.spawn(text(fonts, "from", Type::BODY));
            r.spawn((
                text_field(fonts, &v.dnd_from, 80.0, false),
                ConfigField(DND_FROM),
            ));
            r.spawn(text(fonts, "to", Type::BODY));
            r.spawn((
                text_field(fonts, &v.dnd_to, 80.0, false),
                ConfigField(DND_TO),
            ));
            r.spawn(Node {
                column_gap: px(6),
                ..default()
            })
            .with_children(|days| {
                for day in Weekday::ALL {
                    let on = v.dnd_days.contains(&day);
                    days.spawn((
                        pill(fonts, day, on),
                        setter(DND_DAYS, days_value(&v.dnd_days, day)),
                    ));
                }
            });
        },
        "",
        error,
    );
}

fn permission_row(p: &mut ChildSpawnerCommands, fonts: &UiFonts, status: PermissionStatus) {
    let (word, ink, dot, hint) = match status {
        PermissionStatus::Allowed => ("Allowed", Swatch::Green, Swatch::Green, ""),
        PermissionStatus::Denied => (
            "Off",
            Swatch::Orange,
            Swatch::Orange,
            "macOS is blocking Clúsia's notifications",
        ),
        PermissionStatus::NotDetermined => (
            "Not asked yet",
            Swatch::Muted,
            Swatch::Faint,
            "macOS asks the first time a notification is sent",
        ),
    };
    row(
        p,
        fonts,
        "macOS permission",
        |r| {
            r.spawn(Node {
                column_gap: px(6),
                align_items: AlignItems::Center,
                ..default()
            })
            .with_children(|s| {
                s.spawn((
                    Node {
                        width: px(8),
                        height: px(8),
                        border_radius: BorderRadius::MAX,
                        ..default()
                    },
                    BackgroundColor::default(),
                    Fill(dot),
                ));
                s.spawn(text(fonts, word, Type::STRONG.ink(ink)));
            });
            r.spawn((
                button(fonts, "Open System Settings", Variant::Ghost),
                OpenLink(NOTIFICATION_SETTINGS),
                observe(super::on_link),
            ));
        },
        hint,
        None,
    );
}

/// While the page is open, asks the daemon for the permission now and then: it changes in
/// System Settings, outside the app.
pub fn poll_permission(
    nav: Res<Nav>,
    time: Res<Time<Real>>,
    mut asks: ResMut<crate::bridge::Asks>,
    mut last: Local<Option<f64>>,
) {
    if nav.screen != Screen::Config(Section::Notifications) {
        *last = None;
        return;
    }
    let now = time.elapsed_secs_f64();
    if last.is_none_or(|at| now - at >= PERMISSION_POLL_SECS) {
        *last = Some(now);
        asks.send(Ask::RefreshStatus);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_names_its_event_and_column() {
        for (kind, keys) in KEYS {
            for (column, key) in Column::ALL.into_iter().zip(keys) {
                let name = match column {
                    Column::Tray => "tray",
                    Column::Macos => "macos",
                    Column::Sound => "sound",
                };
                assert_eq!(
                    key,
                    format!("notifications.events.{}.{name}", kind.as_str())
                );
            }
        }
    }

    #[test]
    fn the_defaults_fill_the_mockup_table() {
        let v = view(&Snapshot::default(), &HashMap::new());
        let cells: Vec<(EventKind, [bool; 3])> = v
            .rows
            .iter()
            .map(|r| (r.kind, Column::ALL.map(|c| c.of(r.route))))
            .collect();
        assert_eq!(
            cells,
            [
                (EventKind::ReviewRequested, [true, true, true]),
                (EventKind::CommitsAfterReview, [true, true, false]),
                (EventKind::ReplyToYou, [true, true, false]),
                (EventKind::Mentioned, [true, true, true]),
                (EventKind::AgentFinished, [true, true, false]),
                (EventKind::AgentPermission, [true, true, true]),
                (EventKind::ChecksFailed, [true, false, false]),
                (EventKind::SyncProblem, [true, true, false]),
                (EventKind::StateRecovered, [true, true, false]),
            ]
        );
        assert!(
            v.rows[4].live && !v.rows[5].live,
            "Agent finished is live; the permission row waits for the permission bridge"
        );
        assert_eq!(v.rows[3].hint, "@you in a PR or comment");
        assert_eq!(v.dnd_from, "19:00");
        assert_eq!(v.dnd_to, "09:00");
        assert_eq!(v.permission, PermissionStatus::NotDetermined);
    }

    #[test]
    fn days_toggle_into_a_list_the_daemon_accepts() {
        let days: BTreeSet<Weekday> = [Weekday::Mon, Weekday::Tue].into();
        assert_eq!(days_value(&days, Weekday::Wed), r#"["mon","tue","wed"]"#);
        assert_eq!(days_value(&days, Weekday::Mon), r#"["tue"]"#);
        assert_eq!(days_value(&BTreeSet::new(), Weekday::Sun), r#"["sun"]"#);
    }
}
