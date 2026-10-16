//! The widget kit: themed builders on top of `bevy_ui` and `bevy_ui_widgets` (spec §7.1).
//! Builders return bundles. Colors are `Swatch` markers that `restyle` resolves against the
//! current `Theme`, so a theme switch recolors everything without rebuilding a screen.

use bevy::ecs::spawn::SpawnIter;
use bevy::input::ButtonInput;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::input_focus::{FocusLost, InputFocus};
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::text::{EditableText, FontSource, TextBackgroundColor, TextCursorStyle};
use bevy::ui::Pressed;
use bevy::ui_widgets::Button as WidgetButton;
use bevy::window::{CursorIcon, PrimaryWindow, SystemCursorIcon};

use crate::fonts::UiFonts;
use crate::theme::{Swatch, Theme};

#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fill(pub Swatch);

#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ink(pub Swatch);

#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stroke(pub Swatch);

/// The background behind a run of text (a `TextSpan`'s highlight).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mark(pub Swatch);

/// Shows the pointing-hand cursor while hovered (needs `Hovered`).
#[derive(Component, Debug, Clone, Copy, Default)]
pub struct Clickable;

/// Background while hovered (the `Fill` applies otherwise).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct HoverFill(pub Swatch);

/// Background while pressed, over `HoverFill`.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct PressFill(pub Swatch);

/// A text style: size in points, weight 100–900, color role, font family.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Type {
    pub size: f32,
    pub weight: u16,
    pub ink: Swatch,
    pub mono: bool,
}

impl Type {
    pub const TITLE: Type = Type {
        size: 20.0,
        weight: 600,
        ink: Swatch::Fg,
        mono: false,
    };
    pub const HEADING: Type = Type {
        size: 15.0,
        weight: 600,
        ink: Swatch::Fg,
        mono: false,
    };
    pub const BODY: Type = Type {
        size: 13.0,
        weight: 400,
        ink: Swatch::Fg,
        mono: false,
    };
    pub const STRONG: Type = Type {
        size: 13.0,
        weight: 600,
        ink: Swatch::Fg,
        mono: false,
    };
    pub const MUTED: Type = Type {
        size: 13.0,
        weight: 400,
        ink: Swatch::Muted,
        mono: false,
    };
    pub const META: Type = Type {
        size: 12.0,
        weight: 400,
        ink: Swatch::Faint,
        mono: false,
    };
    pub const MONO: Type = Type {
        size: 12.0,
        weight: 400,
        ink: Swatch::Muted,
        mono: true,
    };
    pub const NUMBER: Type = Type {
        size: 26.0,
        weight: 600,
        ink: Swatch::Fg,
        mono: false,
    };

    pub const fn ink(self, ink: Swatch) -> Type {
        Type { ink, ..self }
    }

    pub const fn size(self, size: f32) -> Type {
        Type { size, ..self }
    }
}

pub fn font(fonts: &UiFonts, t: Type) -> TextFont {
    TextFont {
        font: FontSource::Handle(if t.mono {
            fonts.mono.clone()
        } else {
            fonts.sans.clone()
        }),
        font_size: FontSize::Px(t.size),
        weight: FontWeight(t.weight),
        ..default()
    }
}

pub fn text(fonts: &UiFonts, s: impl Into<String>, t: Type) -> impl Bundle {
    (
        Text::new(s),
        font(fonts, t),
        TextColor::default(),
        Ink(t.ink),
    )
}

pub fn panel(node: Node, fill: Swatch) -> impl Bundle {
    (node, BackgroundColor::default(), Fill(fill))
}

/// A surface with a hairline border and 8 px corners.
pub fn card(node: Node) -> impl Bundle {
    (
        Node {
            border: px(1).all(),
            border_radius: BorderRadius::all(px(8)),
            ..node
        },
        BackgroundColor::default(),
        Fill(Swatch::Surface),
        BorderColor::default(),
        Stroke(Swatch::Line),
    )
}

pub fn divider() -> impl Bundle {
    panel(
        Node {
            height: px(1),
            width: percent(100),
            flex_shrink: 0.0,
            ..default()
        },
        Swatch::Line,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    Primary,
    Secondary,
    Ghost,
    /// A quiet button that reads as a link: the accent ink on no fill.
    Link,
    Danger,
}

/// (fill, hover fill, ink, border) of a button variant.
fn variant_swatches(v: Variant) -> (Swatch, Swatch, Swatch, Swatch) {
    match v {
        Variant::Primary => (
            Swatch::Green,
            Swatch::GreenHover,
            Swatch::OnGreen,
            Swatch::Green,
        ),
        Variant::Secondary => (Swatch::Surface, Swatch::Hover, Swatch::Fg, Swatch::Line),
        Variant::Ghost => (Swatch::Clear, Swatch::Hover, Swatch::Muted, Swatch::Clear),
        Variant::Link => (Swatch::Clear, Swatch::Hover, Swatch::Green, Swatch::Clear),
        Variant::Danger => (
            Swatch::Clear,
            Swatch::OrangeSoft,
            Swatch::Orange,
            Swatch::Clear,
        ),
    }
}

/// The side padding of a one-line label, with the bottom padding that raises the label by
/// `fonts::label_lift` (centring the padded box moves its content up by half the padding).
pub fn label_padding(side: f32, size: f32) -> UiRect {
    UiRect {
        left: px(side),
        right: px(side),
        top: px(0),
        bottom: px(2.0 * crate::fonts::label_lift(size)),
    }
}

/// A 30 px button. Attach `observe(|_: On<Activate>, …| …)` for the action.
pub fn button(fonts: &UiFonts, label: &str, v: Variant) -> impl Bundle {
    let (fill, hover, ink, stroke) = variant_swatches(v);
    (
        Node {
            height: px(30),
            padding: label_padding(12.0, Type::STRONG.size),
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            align_self: AlignSelf::Start,
            border: px(1).all(),
            border_radius: BorderRadius::all(px(6)),
            flex_shrink: 0.0,
            ..default()
        },
        (WidgetButton, Clickable),
        Hovered::default(),
        TabIndex(0),
        BackgroundColor::default(),
        Fill(fill),
        HoverFill(hover),
        PressFill(press_swatch(v)),
        BorderColor::default(),
        Stroke(stroke),
        children![(
            text(fonts, label.to_string(), Type::STRONG.ink(ink)),
            TextLayout::no_wrap(),
        )],
    )
}

/// The fill of a button of variant `v` while pressed: a step past its hover.
fn press_swatch(v: Variant) -> Swatch {
    match v {
        Variant::Primary => Swatch::GreenPress,
        Variant::Danger => Swatch::OrangePress,
        Variant::Secondary | Variant::Ghost => Swatch::Line,
    }
}

/// A glyph-free icon: strokes drawn as boxes, so it is centred by its measured box and does not
/// depend on a font's metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Close,
    Plus,
}

/// One stroke of an `Icon`.
#[derive(Component, Debug, Clone, Copy)]
pub struct IconBar;

/// Side of the square an icon is drawn in, and the width of its strokes.
const ICON_SIDE: f32 = 12.0;
const ICON_STROKE: f32 = 1.5;

/// The strokes of `icon` in `ink`, to spawn as children of a centring box.
fn icon_bars(icon: Icon, ink: Swatch) -> [impl Bundle; 2] {
    let degrees: [f32; 2] = match icon {
        Icon::Close => [45.0, -45.0],
        Icon::Plus => [0.0, 90.0],
    };
    // Both icons use strokes of one length, so the turned × is narrower than the +, as is usual.
    let length = ICON_SIDE;
    degrees.map(|d| {
        (
            IconBar,
            Node {
                position_type: PositionType::Absolute,
                width: px(length),
                height: px(ICON_STROKE),
                left: px((ICON_SIDE - length) / 2.0),
                top: px((ICON_SIDE - ICON_STROKE) / 2.0),
                border_radius: BorderRadius::all(px(ICON_STROKE / 2.0)),
                ..default()
            },
            UiTransform::from_rotation(Rot2::degrees(d)),
            BackgroundColor::default(),
            Fill(ink),
        )
    })
}

/// A square button with only an icon, `side` px on a side, the icon on its centre. It centres
/// itself across its row, unlike `button`.
pub fn icon_button(icon: Icon, v: Variant, side: f32) -> impl Bundle {
    let (fill, hover, ink, stroke) = variant_swatches(v);
    (
        Node {
            width: px(side),
            height: px(side),
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            align_self: AlignSelf::Center,
            border_radius: BorderRadius::all(px(6)),
            flex_shrink: 0.0,
            ..default()
        },
        (WidgetButton, Clickable),
        Hovered::default(),
        TabIndex(0),
        BackgroundColor::default(),
        Fill(fill),
        HoverFill(hover),
        PressFill(press_swatch(v)),
        BorderColor::default(),
        Stroke(stroke),
        children![(
            Node {
                width: px(ICON_SIDE),
                height: px(ICON_SIDE),
                flex_shrink: 0.0,
                ..default()
            },
            Children::spawn(SpawnIter(icon_bars(icon, ink).into_iter())),
        )],
    )
}

/// A button that cannot be used right now: same size, faint, no `Activate`.
pub fn disabled_button(fonts: &UiFonts, label: &str) -> impl Bundle {
    (
        Node {
            height: px(30),
            padding: label_padding(12.0, Type::STRONG.size),
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            align_self: AlignSelf::Start,
            border: px(1).all(),
            border_radius: BorderRadius::all(px(6)),
            flex_shrink: 0.0,
            ..default()
        },
        BackgroundColor::default(),
        Fill(Swatch::Chrome),
        BorderColor::default(),
        Stroke(Swatch::Line),
        children![text(
            fonts,
            label.to_string(),
            Type::STRONG.ink(Swatch::Faint)
        )],
    )
}

/// The track of a segmented control; spawn `segment`s inside it.
pub fn segments() -> impl Bundle {
    panel(
        Node {
            padding: px(3).all(),
            column_gap: px(3),
            border_radius: BorderRadius::all(px(8)),
            align_self: AlignSelf::Start,
            ..default()
        },
        Swatch::Chrome,
    )
}

/// The box of one segment: its label is centred in it on both axes, whatever the width.
pub fn segment_node() -> Node {
    Node {
        height: px(26),
        padding: label_padding(12.0, Type::STRONG.size),
        align_items: AlignItems::Center,
        justify_content: JustifyContent::Center,
        border_radius: BorderRadius::all(px(6)),
        ..default()
    }
}

pub fn segment(fonts: &UiFonts, label: &str, on: bool) -> impl Bundle {
    (
        segment_node(),
        (WidgetButton, Clickable),
        Hovered::default(),
        TabIndex(0),
        BackgroundColor::default(),
        Fill(if on { Swatch::Raised } else { Swatch::Clear }),
        HoverFill(if on { Swatch::Raised } else { Swatch::Hover }),
        children![(
            text(
                fonts,
                label.to_string(),
                if on { Type::STRONG } else { Type::MUTED }
            ),
            TextLayout::no_wrap(),
        )],
    )
}

/// A switch. Activating it should write the opposite value.
pub fn toggle(on: bool) -> impl Bundle {
    (
        Node {
            width: px(30),
            height: px(18),
            padding: px(2).all(),
            border_radius: BorderRadius::MAX,
            justify_content: if on {
                JustifyContent::End
            } else {
                JustifyContent::Start
            },
            flex_shrink: 0.0,
            ..default()
        },
        (WidgetButton, Clickable),
        Hovered::default(),
        TabIndex(0),
        BackgroundColor::default(),
        Fill(if on { Swatch::Green } else { Swatch::Line }),
        HoverFill(if on {
            Swatch::GreenHover
        } else {
            Swatch::Faint
        }),
        children![panel(
            Node {
                width: px(14),
                height: px(14),
                border_radius: BorderRadius::MAX,
                ..default()
            },
            Swatch::Knob,
        )],
    )
}

fn checkbox_box(on: bool, enabled: bool) -> impl Bundle {
    let (fill, stroke) = match (on, enabled) {
        (true, true) => (Swatch::Green, Swatch::Green),
        (true, false) => (Swatch::Line, Swatch::Line),
        (false, true) => (Swatch::Surface, Swatch::Faint),
        (false, false) => (Swatch::Chrome, Swatch::Line),
    };
    (
        Node {
            width: px(18),
            height: px(18),
            border: px(1).all(),
            border_radius: BorderRadius::all(px(4)),
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            flex_shrink: 0.0,
            ..default()
        },
        BackgroundColor::default(),
        Fill(fill),
        BorderColor::default(),
        Stroke(stroke),
        // A turned corner drawn from two borders: it needs no glyph from the bundled fonts.
        Children::spawn(SpawnIter(
            on.then(|| {
                (
                    CheckMark,
                    Node {
                        width: px(5),
                        height: px(9),
                        margin: UiRect::bottom(px(2)),
                        border: UiRect {
                            right: px(2),
                            bottom: px(2),
                            ..default()
                        },
                        ..default()
                    },
                    UiTransform::from_rotation(Rot2::degrees(45.0)),
                    BorderColor::default(),
                    Stroke(Swatch::OnGreen),
                )
            })
            .into_iter(),
        )),
    )
}

/// The white tick inside a checked box.
#[derive(Component, Debug, Clone, Copy)]
pub struct CheckMark;

/// A checkbox. Activating it should write the opposite value.
pub fn checkbox(on: bool) -> impl Bundle {
    (
        checkbox_box(on, true),
        (WidgetButton, Clickable),
        Hovered::default(),
        TabIndex(0),
    )
}

/// A checkbox that cannot be changed: faint, not a button.
pub fn disabled_checkbox(on: bool) -> impl Bundle {
    checkbox_box(on, false)
}

pub fn chip(fonts: &UiFonts, label: &str, selected: bool) -> impl Bundle {
    let (fill, hover, t) = if selected {
        (
            Swatch::GreenSoft,
            Swatch::GreenSoft,
            Type::STRONG.ink(Swatch::Green),
        )
    } else {
        (Swatch::Surface, Swatch::Hover, Type::MUTED)
    };
    (
        Node {
            height: px(26),
            padding: UiRect::horizontal(px(12)),
            align_items: AlignItems::Center,
            border_radius: BorderRadius::MAX,
            flex_shrink: 0.0,
            ..default()
        },
        (WidgetButton, Clickable),
        Hovered::default(),
        TabIndex(0),
        BackgroundColor::default(),
        Fill(fill),
        HoverFill(hover),
        children![text(fonts, label.to_string(), t)],
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tone {
    Green,
    Orange,
    Neutral,
}

impl Tone {
    /// (background, ink)
    pub fn swatches(self) -> (Swatch, Swatch) {
        match self {
            Tone::Green => (Swatch::GreenSoft, Swatch::Green),
            Tone::Orange => (Swatch::OrangeSoft, Swatch::Orange),
            Tone::Neutral => (Swatch::Chrome, Swatch::Muted),
        }
    }
}

/// A small label pill: "moved from 81", "obsolete", "Draft", "not set up".
pub fn badge(fonts: &UiFonts, label: &str, tone: Tone) -> impl Bundle {
    let (fill, ink) = tone.swatches();
    (
        Node {
            height: px(18),
            padding: UiRect::horizontal(px(6)),
            align_items: AlignItems::Center,
            border_radius: BorderRadius::all(px(4)),
            flex_shrink: 0.0,
            ..default()
        },
        BackgroundColor::default(),
        Fill(fill),
        children![text(
            fonts,
            label.to_string(),
            Type {
                size: 11.0,
                weight: 600,
                ink,
                mono: false,
            }
        )],
    )
}

/// The first letter of `login`, upper case (`?` when empty).
pub fn initial(login: &str) -> String {
    login
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_else(|| "?".into())
}

/// A 22 px circle with the person's initial.
pub fn avatar(fonts: &UiFonts, login: &str) -> impl Bundle {
    (
        Node {
            width: px(22),
            height: px(22),
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            border_radius: BorderRadius::MAX,
            flex_shrink: 0.0,
            ..default()
        },
        BackgroundColor::default(),
        Fill(Swatch::Chrome),
        children![text(
            fonts,
            initial(login),
            Type {
                size: 11.0,
                weight: 600,
                ink: Swatch::Muted,
                mono: false,
            }
        )],
    )
}

/// Remembers a text field's last committed value.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub committed: String,
}

/// A text field's value changed and was confirmed (Enter, or focus moved away).
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct FieldCommitted {
    pub entity: Entity,
    pub value: String,
}

/// `Some(value)` (and remembered) when it differs from the last committed value.
pub fn commit(field: &mut Field, value: String) -> Option<String> {
    if value == field.committed {
        return None;
    }
    field.committed = value.clone();
    Some(value)
}

/// A one-line text field, `width` px wide.
pub fn text_field(fonts: &UiFonts, value: &str, width: f32, mono: bool) -> impl Bundle {
    let mut editable = EditableText::new(value);
    editable.allow_newlines = false;
    let t = if mono {
        Type::MONO.ink(Swatch::Fg)
    } else {
        Type::BODY
    };
    (
        Node {
            width: px(width),
            height: px(30),
            padding: UiRect::axes(px(10), px(6)),
            border: px(1).all(),
            border_radius: BorderRadius::all(px(6)),
            flex_shrink: 0.0,
            ..default()
        },
        editable,
        TextLayout::no_wrap(),
        TextCursorStyle::default(),
        font(fonts, t),
        TextColor::default(),
        Ink(t.ink),
        TabIndex(0),
        BackgroundColor::default(),
        Fill(Swatch::Surface),
        BorderColor::default(),
        Stroke(Swatch::Line),
        Field {
            committed: value.to_string(),
        },
    )
}

pub struct KitPlugin;

impl Plugin for KitPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins((
            crate::ui::text_area::TextAreaPlugin,
            crate::ui::leaf::LeafPlugin,
            crate::ui::spinner::SpinnerPlugin,
            crate::ui::markdown::MarkdownPlugin,
            crate::ui::media::MediaPlugin,
        ))
        .add_message::<FieldCommitted>()
        .add_observer(commit_on_blur)
        .add_systems(Update, (commit_on_enter, pointer_cursor))
        .add_systems(PostUpdate, (restyle, hover).chain());
    }
}

fn commit_on_enter(
    keys: Res<ButtonInput<KeyCode>>,
    focus: Res<InputFocus>,
    mut fields: Query<(&mut Field, &EditableText)>,
    mut out: MessageWriter<FieldCommitted>,
) {
    if !keys.just_pressed(KeyCode::Enter) {
        return;
    }
    let Some(entity) = focus.get() else { return };
    if let Ok((mut field, editable)) = fields.get_mut(entity)
        && let Some(value) = commit(&mut field, editable.value().to_string())
    {
        out.write(FieldCommitted { entity, value });
    }
}

fn commit_on_blur(
    lost: On<FocusLost>,
    mut fields: Query<(&mut Field, &EditableText)>,
    mut out: MessageWriter<FieldCommitted>,
) {
    if let Ok((mut field, editable)) = fields.get_mut(lost.entity)
        && let Some(value) = commit(&mut field, editable.value().to_string())
    {
        out.write(FieldCommitted {
            entity: lost.entity,
            value,
        });
    }
}

/// The pointing hand while any `Clickable` is hovered. Writes the window's cursor only when it
/// changes; does nothing without a primary window (headless tests).
fn pointer_cursor(
    mut commands: Commands,
    hovered: Query<&Hovered, With<Clickable>>,
    window: Query<(Entity, Option<&CursorIcon>), With<PrimaryWindow>>,
) {
    let Ok((window, current)) = window.single() else {
        return;
    };
    let want: CursorIcon = if hovered.iter().any(|h| h.get()) {
        SystemCursorIcon::Pointer.into()
    } else {
        SystemCursorIcon::Default.into()
    };
    if current != Some(&want) {
        commands.entity(window).insert(want);
    }
}

/// Resolves swatches: everything on a theme change, otherwise only new or changed markers.
fn restyle(
    theme: Res<Theme>,
    mut fills: Query<(Ref<Fill>, &mut BackgroundColor)>,
    mut inks: Query<(Ref<Ink>, &mut TextColor)>,
    mut strokes: Query<(Ref<Stroke>, &mut BorderColor)>,
    mut marks: Query<(Ref<Mark>, &mut TextBackgroundColor)>,
) {
    let all = theme.is_changed();
    for (mark, mut color) in &mut marks {
        if all || mark.is_changed() {
            color.0 = theme.tokens.get(mark.0);
        }
    }
    for (fill, mut color) in &mut fills {
        if all || fill.is_changed() {
            color.0 = theme.tokens.get(fill.0);
        }
    }
    for (ink, mut color) in &mut inks {
        if all || ink.is_changed() {
            color.0 = theme.tokens.get(ink.0);
        }
    }
    for (stroke, mut color) in &mut strokes {
        if all || stroke.is_changed() {
            *color = BorderColor::all(theme.tokens.get(stroke.0));
        }
    }
}

/// Runs after `restyle`: pressed widgets show `PressFill`, hovered ones `HoverFill`, the others
/// their `Fill`. The press ends without a change the system could see, so it compares colors.
fn hover(
    theme: Res<Theme>,
    mut widgets: Query<(
        Ref<Hovered>,
        &Fill,
        &HoverFill,
        Option<&PressFill>,
        Has<Pressed>,
        &mut BackgroundColor,
    )>,
) {
    for (hovered, fill, over, press, pressed, mut color) in &mut widgets {
        // Only widgets with a `PressFill` need the check every frame.
        if press.is_none() && !theme.is_changed() && !hovered.is_changed() {
            continue;
        }
        let swatch = match press {
            Some(press) if pressed => press.0,
            _ if hovered.get() => over.0,
            _ => fill.0,
        };
        let want = theme.tokens.get(swatch);
        if color.0 != want {
            color.0 = want;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::Snapshot;
    use crate::testing;
    use crate::theme::{DARK, LIGHT};
    use bevy::window::{WindowTheme, WindowThemeChanged};

    fn bg(app: &mut App, e: Entity) -> Color {
        app.world().get::<BackgroundColor>(e).unwrap().0
    }

    #[test]
    fn only_a_checked_box_has_a_tick() {
        let mut app = testing::app(Snapshot::default());
        app.world_mut().spawn(checkbox(true));
        app.world_mut().spawn(checkbox(false));
        app.world_mut().spawn(disabled_checkbox(true));
        app.update();
        let ticks = app
            .world_mut()
            .query::<&CheckMark>()
            .iter(app.world())
            .count();
        assert_eq!(ticks, 2);
    }

    #[test]
    fn restyle_recolors_without_rebuilding() {
        let mut app = testing::app(Snapshot::default());
        let fonts = UiFonts::default();
        let panel = app
            .world_mut()
            .spawn(panel(Node::default(), Swatch::Surface))
            .id();
        let label = app.world_mut().spawn(text(&fonts, "hi", Type::MUTED)).id();
        app.update();
        assert_eq!(bg(&mut app, panel), LIGHT.surface);
        assert_eq!(app.world().get::<TextColor>(label).unwrap().0, LIGHT.muted);
        testing::set_config_locally(&mut app, "appearance.theme", "dark");
        assert_eq!(bg(&mut app, panel), DARK.surface, "same entity, new color");
        assert_eq!(app.world().get::<TextColor>(label).unwrap().0, DARK.muted);
    }

    #[test]
    fn theme_follows_config_and_system() {
        let mut app = testing::app(Snapshot::default());
        assert!(
            !app.world().resource::<Theme>().dark,
            "System + light macOS"
        );
        app.world_mut().write_message(WindowThemeChanged {
            window: Entity::PLACEHOLDER,
            theme: WindowTheme::Dark,
        });
        app.update();
        assert!(app.world().resource::<Theme>().dark, "System follows macOS");
        assert_eq!(app.world().resource::<ClearColor>().0, DARK.bg);
        testing::set_config_locally(&mut app, "appearance.theme", "light");
        assert!(
            !app.world().resource::<Theme>().dark,
            "an explicit choice wins"
        );
        testing::set_config_locally(&mut app, "appearance.code_size", "16");
        assert_eq!(app.world().resource::<Theme>().code_size, 16.0);
    }

    #[test]
    fn hover_uses_the_new_tokens() {
        let mut app = testing::app(Snapshot::default());
        let fonts = UiFonts::default();
        let b = app
            .world_mut()
            .spawn(button(&fonts, "Sync now", Variant::Secondary))
            .id();
        app.update();
        assert_eq!(bg(&mut app, b), LIGHT.surface);
        app.world_mut().entity_mut(b).insert(Hovered(true));
        app.update();
        assert_eq!(bg(&mut app, b), LIGHT.hover);
        testing::set_config_locally(&mut app, "appearance.theme", "dark");
        assert_eq!(bg(&mut app, b), DARK.hover, "still hovered, new theme");
        app.world_mut().entity_mut(b).insert(Hovered(false));
        app.update();
        assert_eq!(bg(&mut app, b), DARK.surface);
    }

    #[test]
    fn fields_commit_only_changes() {
        let mut f = Field {
            committed: "a".into(),
        };
        assert_eq!(commit(&mut f, "a".into()), None);
        assert_eq!(commit(&mut f, "b".into()), Some("b".into()));
        assert_eq!(f.committed, "b");
        assert_eq!(commit(&mut f, "b".into()), None);
    }

    #[test]
    fn builders_carry_the_widget_parts() {
        let mut app = testing::app(Snapshot::default());
        let fonts = UiFonts::default();
        let field = app
            .world_mut()
            .spawn(text_field(&fonts, "60", 64.0, false))
            .id();
        let on = app.world_mut().spawn(toggle(true)).id();
        let seg = app.world_mut().spawn(segment(&fonts, "Dark", true)).id();
        app.update();
        let w = app.world();
        assert_eq!(
            w.get::<EditableText>(field).unwrap().value().to_string(),
            "60"
        );
        assert_eq!(w.get::<Field>(field).unwrap().committed, "60");
        assert!(w.get::<WidgetButton>(on).is_some());
        assert_eq!(w.get::<BackgroundColor>(on).unwrap().0, LIGHT.green);
        assert_eq!(w.get::<BackgroundColor>(seg).unwrap().0, LIGHT.raised);
    }

    #[test]
    fn badges_and_avatars() {
        let mut app = testing::app(Snapshot::default());
        let fonts = UiFonts::default();
        let moved = app
            .world_mut()
            .spawn(badge(&fonts, "moved from 81", Tone::Green))
            .id();
        let obsolete = app
            .world_mut()
            .spawn(badge(&fonts, "obsolete", Tone::Orange))
            .id();
        let face = app.world_mut().spawn(avatar(&fonts, "mona")).id();
        app.update();
        assert_eq!(bg(&mut app, moved), LIGHT.green_soft);
        assert_eq!(bg(&mut app, obsolete), LIGHT.orange_soft);
        let label = app.world().get::<Children>(obsolete).unwrap()[0];
        assert_eq!(app.world().get::<Text>(label).unwrap().0, "obsolete");
        assert_eq!(app.world().get::<TextColor>(label).unwrap().0, LIGHT.orange);
        let letter = app.world().get::<Children>(face).unwrap()[0];
        assert_eq!(app.world().get::<Text>(letter).unwrap().0, "M");
        assert_eq!(app.world().get::<Node>(face).unwrap().width, px(22));
        assert_eq!(initial("@octo"), "O");
        assert_eq!(initial(""), "?");
        assert_eq!(initial("élan"), "É");
    }

    #[test]
    fn hovering_a_clickable_shows_the_pointer() {
        let mut app = testing::app(Snapshot::default());
        let fonts = UiFonts::default();
        let window = app
            .world_mut()
            .spawn((Window::default(), PrimaryWindow))
            .id();
        let b = app
            .world_mut()
            .spawn(button(&fonts, "Reply", Variant::Ghost))
            .id();
        app.update();
        let cursor = |app: &App| app.world().get::<CursorIcon>(window).cloned();
        assert_eq!(cursor(&app), Some(SystemCursorIcon::Default.into()));
        app.world_mut().entity_mut(b).insert(Hovered(true));
        app.update();
        assert_eq!(cursor(&app), Some(SystemCursorIcon::Pointer.into()));
        app.world_mut().entity_mut(b).insert(Hovered(false));
        app.update();
        assert_eq!(cursor(&app), Some(SystemCursorIcon::Default.into()));
    }

    #[test]
    fn disabled_buttons_cannot_be_activated() {
        let mut app = testing::app(Snapshot::default());
        let fonts = UiFonts::default();
        let off = app
            .world_mut()
            .spawn(disabled_button(&fonts, "Publish to GitHub"))
            .id();
        app.update();
        assert!(app.world().get::<WidgetButton>(off).is_none());
        assert!(app.world().get::<Clickable>(off).is_none());
        assert_eq!(bg(&mut app, off), LIGHT.chrome);
    }

    fn laid_out(app: &mut App, bundle: impl Bundle) -> Entity {
        let root = app
            .world_mut()
            .spawn(Node {
                padding: UiRect::all(px(20)),
                column_gap: px(10),
                ..default()
            })
            .id();
        let e = app.world_mut().spawn((bundle, ChildOf(root))).id();
        app.update();
        app.update();
        e
    }

    fn bars_of(app: &mut App, button: Entity) -> Vec<Entity> {
        let mut bars = app.world_mut().query::<(Entity, &IconBar)>();
        bars.iter(app.world())
            .filter(|(e, _)| {
                let mut at = *e;
                while let Some(parent) = app.world().get::<ChildOf>(at) {
                    at = parent.parent();
                    if at == button {
                        return true;
                    }
                }
                false
            })
            .map(|(e, _)| e)
            .collect()
    }

    #[test]
    fn an_icon_sits_on_the_centre_of_its_button() {
        let mut app = testing::layout_app(Snapshot::default(), UVec2::new(800, 600));
        for side in [24.0, 28.0, 30.0] {
            for which in [Icon::Close, Icon::Plus] {
                let b = laid_out(&mut app, icon_button(which, Variant::Ghost, side));
                let (_, _, w, h) = testing::frame(&app, b);
                assert_eq!((w, h), (side, side), "a square button");
                let bars = bars_of(&mut app, b);
                assert_eq!(bars.len(), 2, "two strokes make the icon");
                let want = testing::center(&app, b);
                for bar in bars {
                    let got = testing::center(&app, bar);
                    assert!(
                        (got - want).abs().max_element() < 0.01,
                        "{which:?} in {side}: stroke at {got}, button at {want}"
                    );
                }
            }
        }
    }

    #[test]
    fn segment_labels_and_pills_sit_in_the_middle_of_their_segments() {
        let mut app = testing::layout_app(Snapshot::default(), UVec2::new(800, 600));
        let fonts = UiFonts::default();
        let track = laid_out(&mut app, segments());
        let labels = ["Unified", "Split", "Story", "Calls"];
        let segs: Vec<Entity> = labels
            .iter()
            .enumerate()
            .map(|(i, l)| {
                app.world_mut()
                    .spawn((segment(&fonts, l, i == 0), ChildOf(track)))
                    .id()
            })
            .collect();
        app.update();
        app.update();
        let (tx, ty, tw, th) = testing::frame(&app, track);
        let (sx, sy, _, sh) = testing::frame(&app, segs[0]);
        assert_eq!((sx - tx, sy - ty), (3.0, 3.0), "the pill is inset evenly");
        assert_eq!(th - sh - (sy - ty), 3.0, "and has the same room below");
        let (lx, _, lw, _) = testing::frame(&app, *segs.last().unwrap());
        assert_eq!(tx + tw - (lx + lw), 3.0, "and after the last segment");
        for seg in segs {
            let label = app.world().get::<Children>(seg).unwrap()[0];
            let (l, s) = (testing::center(&app, label), testing::center(&app, seg));
            assert_eq!(l.x, s.x, "centred across");
            // Raised by what the font's line box puts below the letters' middle (a pixel at most).
            assert!(
                s.y - l.y >= 0.0 && s.y - l.y <= 1.0,
                "label at {l}, segment at {s}"
            );
        }
    }

    #[test]
    fn primary_and_danger_buttons_show_a_press_too() {
        use bevy::ui::Pressed;
        let mut app = testing::app(Snapshot::default());
        let fonts = UiFonts::default();
        for (v, hover, press) in [
            (Variant::Primary, LIGHT.green_hover, LIGHT.green_press),
            (Variant::Danger, LIGHT.orange_soft, LIGHT.orange_press),
        ] {
            let b = app.world_mut().spawn(button(&fonts, "Go", v)).id();
            app.update();
            app.world_mut().entity_mut(b).insert(Hovered(true));
            app.update();
            assert_eq!(bg(&mut app, b), hover);
            app.world_mut().entity_mut(b).insert(Pressed);
            app.update();
            assert_eq!(bg(&mut app, b), press, "{v:?}");
            assert_ne!(hover, press);
        }
    }

    #[test]
    fn a_pressed_button_darkens_past_its_hover() {
        use bevy::ui::Pressed;
        let mut app = testing::app(Snapshot::default());
        let fonts = UiFonts::default();
        let b = app
            .world_mut()
            .spawn(button(&fonts, "Config", Variant::Secondary))
            .id();
        app.update();
        app.world_mut().entity_mut(b).insert(Hovered(true));
        app.update();
        assert_eq!(bg(&mut app, b), LIGHT.hover);
        app.world_mut().entity_mut(b).insert(Pressed);
        app.update();
        assert_eq!(bg(&mut app, b), LIGHT.line);
        app.world_mut().entity_mut(b).remove::<Pressed>();
        app.update();
        assert_eq!(bg(&mut app, b), LIGHT.hover);
    }
}
