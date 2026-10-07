//! Helpers for tests that open a composer on the demo review.

use std::ops::Range;

use bevy::prelude::*;
use bevy::text::{EditableText, FontCx, LayoutCx};
use clusia_core::PrRef;

use crate::review_state::{EditTarget, Editor, ReviewTabs};
use crate::testing;
use crate::ui::composer::{ComposerArea, ComposerMode, ComposerModeButton, read_area, write_area};

pub(crate) fn open(app: &mut App, target: EditTarget, text: &str) -> (PrRef, Entity) {
    let pr = testing::open_ready(app, false);
    app.world_mut()
        .resource_mut::<ReviewTabs>()
        .0
        .get_mut(&pr)
        .unwrap()
        .ui
        .editor = Some(Editor {
        target,
        text: text.into(),
        error: None,
        ticket: None,
        mode: ComposerMode::Write,
    });
    testing::settle(app);
    let area = testing::find::<ComposerArea>(app, |_| true);
    (pr, area)
}

pub(crate) fn value(app: &App, area: Entity) -> String {
    app.world()
        .get::<EditableText>(area)
        .unwrap()
        .value()
        .to_string()
}

pub(crate) fn editor_of(app: &App, pr: &PrRef) -> Editor {
    app.world().resource::<ReviewTabs>().0[pr]
        .ui
        .editor
        .clone()
        .unwrap()
}

pub(crate) fn mode_button(app: &mut App, mode: ComposerMode) -> Entity {
    testing::find::<ComposerModeButton>(app, |b| b.mode == mode)
}

pub(crate) fn display(app: &App, e: Entity) -> Display {
    app.world().get::<Node>(e).unwrap().display
}

/// The app's text contexts know Inter, as in the running window, so selections are laid out.
pub(crate) fn with_fonts(app: &mut App) {
    let mut fonts = app.world_mut().resource_mut::<FontCx>();
    let family = fonts
        .collection
        .register_fonts(Font::from_bytes(crate::fonts::INTER.to_vec()).data, None)[0]
        .0;
    let name = fonts.collection.family_name(family).unwrap().to_string();
    fonts.set_sans_serif_family(&name).unwrap();
}

pub(crate) fn select(app: &mut App, area: Entity, range: Range<usize>) {
    let (text, _) = read_area(app.world().get::<EditableText>(area).unwrap());
    let mut fonts = app.world_mut().remove_resource::<FontCx>().unwrap();
    let mut layout = app.world_mut().remove_resource::<LayoutCx>().unwrap();
    let mut editable = app.world_mut().get_mut::<EditableText>(area).unwrap();
    write_area(&mut editable, &mut fonts, &mut layout, &text, range);
    app.world_mut().insert_resource(fonts);
    app.world_mut().insert_resource(layout);
}
