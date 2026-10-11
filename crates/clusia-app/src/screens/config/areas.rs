//! Config › Harness: the audit areas. Each area has a switch; a custom one can be edited and
//! deleted, and **+ Add area** opens a small form. Every change writes the whole list.

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::ecs::query::QueryFilter;
use bevy::prelude::*;
use bevy::text::EditableText;
use bevy::ui_widgets::{Activate, observe};
use clusia_core::checks::{
    AREA_INSTRUCTION_MAX, AREA_NAME_MAX, AuditArea, is_builtin, slug_id, validate_area,
};

use super::{FieldError, row};
use crate::bridge::{Asks, Model, set_config};
use crate::fonts::UiFonts;
use crate::nav::{Nav, Screen, Section};
use crate::theme::Swatch;
use crate::ui::kit::{
    FieldCommitted, Type, Variant, button, card, checkbox, disabled_button, text, text_field,
};

/// The config key that holds the list.
pub const AREAS_KEY: &str = "harness.audit_areas";

/// How many characters of an area's instruction its row shows.
const SHOWN_INSTRUCTION: usize = 110;

/// What the open form edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormTarget {
    New,
    /// The id of a custom area.
    Edit(String),
}

/// The add/edit form: closed while `target` is `None`. `name` and `instruction` fill the
/// fields when it opens or after a refused save; `error` is the reason for that refusal.
///
/// `writing` is the list last sent and not yet back from the daemon (in live mode a write
/// comes back only after a round trip). The switches are drawn from it, and every change
/// builds on it, so two quick changes never undo each other. `saving` keeps the form open
/// until its list is back, so a refused write loses nothing that was typed.
#[derive(Resource, Debug, Default, Clone, PartialEq, Eq)]
pub struct AreaForm {
    pub target: Option<FormTarget>,
    pub name: String,
    pub instruction: String,
    pub error: Option<String>,
    pub saving: bool,
    pub writing: Option<Vec<AuditArea>>,
}

impl AreaForm {
    /// The list a change builds on: the one being written, else the daemon's.
    fn base(&self, model: &Model) -> Vec<AuditArea> {
        self.writing
            .clone()
            .unwrap_or_else(|| model.snapshot.config.harness.audit_areas.clone())
    }

    /// Sends `next` and remembers it until the daemon answers.
    fn write(&mut self, asks: &mut Asks, model: &mut Model, next: Vec<AuditArea>) {
        set_config(asks, model, AREAS_KEY, areas_value(&next));
        self.writing = Some(next);
    }

    /// A closed form that keeps the write in flight.
    fn closed(&self) -> AreaForm {
        AreaForm {
            writing: self.writing.clone(),
            ..default()
        }
    }
}

/// The checkbox that switches an area (by id) on or off.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct AreaSwitch(pub String);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct EditArea(pub String);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct DeleteArea(pub String);

#[derive(Component, Debug)]
pub struct AddAreaButton;

#[derive(Component, Debug)]
pub struct SaveArea;

#[derive(Component, Debug)]
pub struct CancelArea;

#[derive(Component, Debug)]
pub struct AreaNameField;

#[derive(Component, Debug)]
pub struct AreaInstructionField;

/// `areas` as the value of `harness.audit_areas`.
pub fn areas_value(areas: &[AuditArea]) -> String {
    serde_json::to_string(areas).expect("audit areas serialize")
}

/// `areas` with the switch of `id` flipped.
pub fn toggled(areas: &[AuditArea], id: &str) -> Vec<AuditArea> {
    let mut next = areas.to_vec();
    if let Some(area) = next.iter_mut().find(|a| a.id == id) {
        area.enabled = !area.enabled;
    }
    next
}

/// `areas` without the custom area `id`; a built-in area (by id) always stays.
pub fn without(areas: &[AuditArea], id: &str) -> Vec<AuditArea> {
    areas
        .iter()
        .filter(|a| is_builtin(&a.id) || a.id != id)
        .cloned()
        .collect()
}

/// The list after saving the form, or why the form is refused.
pub fn saved(
    areas: &[AuditArea],
    target: &FormTarget,
    name: &str,
    instruction: &str,
) -> Result<Vec<AuditArea>, String> {
    let (name, instruction) = (name.trim(), instruction.trim());
    let mut next = areas.to_vec();
    match target {
        FormTarget::New => {
            let taken: Vec<String> = areas.iter().map(|a| a.id.clone()).collect();
            let area = AuditArea {
                id: slug_id(name, &taken),
                name: name.to_string(),
                instruction: instruction.to_string(),
                enabled: true,
                builtin: false,
            };
            validate_area(&area)?;
            next.push(area);
        }
        FormTarget::Edit(id) => {
            let area = next
                .iter_mut()
                .find(|a| a.id == *id && !is_builtin(&a.id))
                .ok_or_else(|| "that area no longer exists".to_string())?;
            area.name = name.to_string();
            area.instruction = instruction.to_string();
            validate_area(area)?;
        }
    }
    Ok(next)
}

/// `text` cut to `max` characters, with an ellipsis when it was longer.
fn short(text: &str, max: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        return one_line;
    }
    let cut: String = one_line.chars().take(max).collect();
    format!("{}…", cut.trim_end())
}

/// The **Audit areas** section: the list, its refusal, and the form or **+ Add area**.
pub fn build(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    areas: &[AuditArea],
    error: &Option<String>,
    form: &Option<AreaForm>,
) {
    super::heading(p, fonts, "Audit areas");
    p.spawn(text(
        fonts,
        "Claude Code audits the change one area at a time. A built-in area can be switched off but not deleted.",
        Type::MUTED,
    ));
    p.spawn(Node {
        flex_direction: FlexDirection::Column,
        row_gap: px(10),
        max_width: px(760),
        ..default()
    })
    .with_children(|c| {
        for area in areas {
            area_row(c, fonts, area);
        }
    });
    if let Some(message) = error {
        p.spawn((
            FieldError(AREAS_KEY),
            children![text(fonts, message.clone(), Type::BODY.ink(Swatch::Orange))],
        ));
    }
    match form {
        Some(f) => form_card(p, fonts, f),
        None => {
            p.spawn((
                button(fonts, "+ Add area", Variant::Secondary),
                AddAreaButton,
                observe(on_add),
            ));
        }
    }
}

fn area_row(p: &mut ChildSpawnerCommands, fonts: &UiFonts, area: &AuditArea) {
    p.spawn(Node {
        column_gap: px(10),
        align_items: AlignItems::Center,
        ..default()
    })
    .with_children(|r| {
        r.spawn((
            checkbox(area.enabled),
            AreaSwitch(area.id.clone()),
            observe(on_switch),
        ));
        r.spawn(Node {
            flex_grow: 1.0,
            flex_basis: px(0),
            min_width: px(0),
            flex_direction: FlexDirection::Column,
            row_gap: px(2),
            ..default()
        })
        .with_children(|t| {
            let name = if area.enabled {
                Type::STRONG
            } else {
                Type::STRONG.ink(Swatch::Faint)
            };
            t.spawn(text(fonts, area.name.clone(), name));
            t.spawn(text(
                fonts,
                short(&area.instruction, SHOWN_INSTRUCTION),
                Type::META,
            ));
        });
        if !is_builtin(&area.id) {
            r.spawn((
                button(fonts, "Edit", Variant::Secondary),
                EditArea(area.id.clone()),
                observe(on_edit),
            ));
            r.spawn((
                button(fonts, "Delete", Variant::Danger),
                DeleteArea(area.id.clone()),
                observe(on_delete),
            ));
        }
    });
}

fn form_card(p: &mut ChildSpawnerCommands, fonts: &UiFonts, f: &AreaForm) {
    let title = match f.target {
        Some(FormTarget::Edit(_)) => "Edit audit area",
        _ => "Add audit area",
    };
    p.spawn(card(Node {
        flex_direction: FlexDirection::Column,
        row_gap: px(10),
        max_width: px(760),
        padding: UiRect::axes(px(16), px(14)),
        ..default()
    }))
    .with_children(|c| {
        c.spawn(text(fonts, title, Type::STRONG));
        row(
            c,
            fonts,
            "Name",
            |r| {
                r.spawn((text_field(fonts, &f.name, 380.0, false), AreaNameField));
            },
            &format!("Up to {AREA_NAME_MAX} characters"),
            None,
        );
        row(
            c,
            fonts,
            "Instruction",
            |r| {
                r.spawn((
                    text_field(fonts, &f.instruction, 380.0, false),
                    AreaInstructionField,
                ));
            },
            &format!("What to look for, up to {AREA_INSTRUCTION_MAX} characters"),
            None,
        );
        if let Some(message) = &f.error {
            c.spawn(text(fonts, message.clone(), Type::BODY.ink(Swatch::Orange)));
        }
        c.spawn(Node {
            column_gap: px(8),
            ..default()
        })
        .with_children(|b| {
            if f.saving {
                b.spawn((disabled_button(fonts, "Saving…"), SaveArea));
            } else {
                b.spawn((
                    button(fonts, "Save area", Variant::Primary),
                    SaveArea,
                    observe(on_save),
                ));
            }
            b.spawn((
                button(fonts, "Cancel", Variant::Secondary),
                CancelArea,
                observe(on_cancel),
            ));
        });
    });
}

fn on_add(_activate: On<Activate>, mut form: ResMut<AreaForm>) {
    *form = AreaForm {
        target: Some(FormTarget::New),
        ..form.closed()
    };
}

/// Flips one area's switch, on the list as it is being written.
fn on_switch(
    activate: On<Activate>,
    switches: Query<&AreaSwitch>,
    mut form: ResMut<AreaForm>,
    mut model: ResMut<Model>,
    mut asks: ResMut<Asks>,
) {
    let Ok(AreaSwitch(id)) = switches.get(activate.entity) else {
        return;
    };
    let next = toggled(&form.base(&model), id);
    form.write(&mut asks, &mut model, next);
}

fn on_edit(
    activate: On<Activate>,
    targets: Query<&EditArea>,
    model: Res<Model>,
    mut form: ResMut<AreaForm>,
) {
    let Ok(EditArea(id)) = targets.get(activate.entity) else {
        return;
    };
    let areas = form.base(&model);
    if let Some(area) = areas.iter().find(|a| a.id == *id && !is_builtin(&a.id)) {
        *form = AreaForm {
            target: Some(FormTarget::Edit(id.clone())),
            name: area.name.clone(),
            instruction: area.instruction.clone(),
            ..form.closed()
        };
    }
}

fn on_delete(
    activate: On<Activate>,
    targets: Query<&DeleteArea>,
    mut form: ResMut<AreaForm>,
    mut model: ResMut<Model>,
    mut asks: ResMut<Asks>,
) {
    let Ok(DeleteArea(id)) = targets.get(activate.entity) else {
        return;
    };
    let areas = form.base(&model);
    let next = without(&areas, id);
    if next.len() == areas.len() {
        return;
    }
    if form.target == Some(FormTarget::Edit(id.clone())) {
        *form = form.closed();
    }
    form.write(&mut asks, &mut model, next);
}

fn on_save(
    _activate: On<Activate>,
    names: Query<&EditableText, With<AreaNameField>>,
    instructions: Query<&EditableText, With<AreaInstructionField>>,
    mut form: ResMut<AreaForm>,
    mut model: ResMut<Model>,
    mut asks: ResMut<Asks>,
) {
    let Some(target) = form.target.clone() else {
        return;
    };
    // The fields are read as they stand: a click on **Save area** may come before the field
    // has reported its text.
    let (name, instruction) = (first_text(&names), first_text(&instructions));
    let saving = saved(&form.base(&model), &target, &name, &instruction);
    *form = AreaForm {
        target: Some(target),
        name: name.trim().to_string(),
        instruction: instruction.trim().to_string(),
        error: saving.as_ref().err().cloned(),
        saving: saving.is_ok(),
        ..form.closed()
    };
    if let Ok(next) = saving {
        form.write(&mut asks, &mut model, next);
    }
}

fn on_cancel(_activate: On<Activate>, mut form: ResMut<AreaForm>) {
    *form = form.closed();
}

/// Ends the write in flight: once the daemon's list is the one sent, the write is done and a
/// saving form closes; once the daemon refused it, the switches show the daemon's list again
/// and the form stays open with what was typed (the refusal shows under the list).
pub fn settle_area_write(model: Res<Model>, mut form: ResMut<AreaForm>) {
    let Some(writing) = &form.writing else {
        return;
    };
    if model.snapshot.config.harness.audit_areas == *writing {
        if form.saving {
            *form = AreaForm::default();
        } else {
            form.writing = None;
        }
    } else if model.rejected.contains_key(AREAS_KEY) {
        form.writing = None;
        form.saving = false;
    }
}

/// Keeps what was typed when the page is rebuilt for another reason (a probe result, a change
/// made at the terminal): a field's committed text goes into the resource the rebuild reads.
pub fn keep_form_text(
    mut commits: MessageReader<FieldCommitted>,
    names: Query<(), With<AreaNameField>>,
    instructions: Query<(), With<AreaInstructionField>>,
    mut form: ResMut<AreaForm>,
) {
    for commit in commits.read() {
        if form.target.is_none() {
            continue;
        }
        if names.contains(commit.entity) {
            form.name = commit.value.clone();
        } else if instructions.contains(commit.entity) {
            form.instruction = commit.value.clone();
        }
    }
}

/// Closes the form when the Harness page is not the one showing, so a half-open form does not
/// come back on a later visit.
pub fn reset_form_off_page(nav: Res<Nav>, mut form: ResMut<AreaForm>) {
    if nav.screen != Screen::Config(Section::Harness) && *form != form.closed() {
        *form = form.closed();
    }
}

/// The text of the one field `q` selects.
fn first_text<F: QueryFilter>(q: &Query<&EditableText, F>) -> String {
    q.iter()
        .next()
        .map(|t| t.value().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{Ask, Model};
    use crate::fixture;
    use crate::nav::{Nav, Section};
    use crate::testing::{self, NOW};
    use crate::ui::kit::Field;
    use clusia_core::checks::default_areas;
    use clusia_protocol::WindowTarget;

    fn harness_app() -> App {
        let mut app = testing::app(fixture::demo(NOW));
        app.world_mut()
            .resource_mut::<Nav>()
            .go(&WindowTarget::Config);
        app.world_mut()
            .resource_mut::<Nav>()
            .open_section(Section::Harness);
        testing::settle(&mut app);
        app
    }

    fn custom(id: &str, name: &str) -> AuditArea {
        AuditArea {
            id: id.into(),
            name: name.into(),
            instruction: "Check every migration can be undone.".into(),
            enabled: true,
            builtin: false,
        }
    }

    /// The list a recorded `harness.audit_areas` write carries.
    fn written_areas(asks: &[Ask]) -> Vec<AuditArea> {
        let value = asks
            .iter()
            .find_map(|a| match a {
                Ask::SetConfig { key, value } if key == AREAS_KEY => Some(value.clone()),
                _ => None,
            })
            .expect("a write of the audit areas");
        serde_json::from_str(&value).expect("the list is JSON")
    }

    fn with_custom(app: &mut App) -> Vec<AuditArea> {
        let mut areas = default_areas();
        areas.push(custom("migrations", "Migrations"));
        testing::set_config_locally(app, AREAS_KEY, &areas_value(&areas));
        testing::settle(app);
        areas
    }

    fn type_form(app: &mut App, name: &str, instruction: &str) {
        let n = testing::find::<AreaNameField>(app, |_| true);
        testing::type_into(app, n, name);
        let i = testing::find::<AreaInstructionField>(app, |_| true);
        testing::type_into(app, i, instruction);
    }

    #[test]
    fn a_new_area_gets_a_unique_slug() {
        let areas = default_areas();
        let next = saved(
            &areas,
            &FormTarget::New,
            "  Migrations ",
            " Check every migration can be undone. ",
        )
        .unwrap();
        assert_eq!(next.len(), 7);
        let added = next.last().unwrap();
        assert_eq!(
            (
                added.id.as_str(),
                added.name.as_str(),
                added.instruction.as_str()
            ),
            (
                "migrations",
                "Migrations",
                "Check every migration can be undone."
            )
        );
        assert!(added.enabled && !added.builtin);
        let again = saved(&next, &FormTarget::New, "Migrations", "Same, again.").unwrap();
        assert_eq!(again.last().unwrap().id, "migrations-2");
        assert_eq!(next[..6], areas[..], "the built-in areas stay as they were");
    }

    #[test]
    fn a_form_the_core_refuses_says_why() {
        let areas = default_areas();
        let long = |n: usize| "x".repeat(n);
        assert_eq!(
            saved(&areas, &FormTarget::New, "  ", "Check it."),
            Err("the name is empty".to_string())
        );
        assert!(saved(&areas, &FormTarget::New, &long(41), "Check it.").is_err());
        assert!(saved(&areas, &FormTarget::New, "Name", "").is_err());
        assert!(saved(&areas, &FormTarget::New, "Name", &long(501)).is_err());
        assert!(saved(&areas, &FormTarget::New, &long(40), &long(500)).is_ok());
    }

    #[test]
    fn only_a_custom_area_is_edited_or_deleted() {
        let mut areas = default_areas();
        areas.push(custom("migrations", "Migrations"));
        areas[6].enabled = false;
        let edited = saved(
            &areas,
            &FormTarget::Edit("migrations".into()),
            "Schema changes",
            "Check every schema change can be undone.",
        )
        .unwrap();
        assert_eq!(edited[6].id, "migrations", "an edit keeps the id");
        assert_eq!(edited[6].name, "Schema changes");
        assert!(!edited[6].enabled, "and the switch");
        assert!(saved(&areas, &FormTarget::Edit("docs".into()), "Docs", "Docs.").is_err());
        assert!(saved(&areas, &FormTarget::Edit("gone".into()), "Docs", "Docs.").is_err());
        assert_eq!(without(&areas, "migrations").len(), 6);
        assert_eq!(without(&areas, "docs").len(), 7, "a built-in area stays");
        let flipped = toggled(&areas, "tests");
        assert!(!flipped[4].enabled && flipped[0].enabled);
        assert_eq!(toggled(&areas, "gone"), areas);
    }

    #[test]
    fn the_page_lists_the_six_areas_with_switches() {
        let mut app = harness_app();
        for needle in [
            "Audit areas",
            "Correctness",
            "Concurrency",
            "Error handling",
            "Performance",
            "Tests",
            "Docs and changelog",
            "+ Add area",
        ] {
            assert!(testing::shows(&mut app, needle), "{needle}");
        }
        assert_eq!(testing::count::<AreaSwitch>(&mut app), 6);
        assert_eq!(testing::count::<EditArea>(&mut app), 0);
        assert_eq!(testing::count::<DeleteArea>(&mut app), 0);
    }

    #[test]
    fn a_switch_writes_the_whole_list() {
        let mut app = harness_app();
        let concurrency = testing::find::<AreaSwitch>(&mut app, |s| s.0 == "concurrency");
        testing::activate(&mut app, concurrency);
        let asks = testing::recorded(&mut app);
        assert_eq!(asks.len(), 1);
        let areas = written_areas(&asks);
        assert_eq!(areas.len(), 6);
        assert!(areas.iter().all(|a| a.enabled == (a.id != "concurrency")));
    }

    #[test]
    fn adding_an_area_sends_the_list_with_it() {
        let mut app = harness_app();
        let add = testing::find::<AddAreaButton>(&mut app, |_| true);
        testing::activate(&mut app, add);
        testing::settle(&mut app);
        assert_eq!(
            app.world().resource::<AreaForm>().target,
            Some(FormTarget::New)
        );
        assert!(testing::shows(&mut app, "Add audit area"));
        type_form(
            &mut app,
            "Migrations",
            "Check every migration can be undone.",
        );
        let save = testing::find::<SaveArea>(&mut app, |_| true);
        testing::activate(&mut app, save);
        let areas = written_areas(&testing::recorded(&mut app));
        assert_eq!(areas.len(), 7);
        let added = &areas[6];
        assert_eq!(
            (added.id.as_str(), added.name.as_str()),
            ("migrations", "Migrations")
        );
        assert!(added.enabled && !added.builtin);
    }

    #[test]
    fn a_refused_form_stays_open_and_sends_nothing() {
        let mut app = harness_app();
        let add = testing::find::<AddAreaButton>(&mut app, |_| true);
        testing::activate(&mut app, add);
        testing::settle(&mut app);
        type_form(&mut app, "   ", "Check it.");
        let save = testing::find::<SaveArea>(&mut app, |_| true);
        testing::activate(&mut app, save);
        testing::settle(&mut app);
        assert!(testing::recorded(&mut app).is_empty());
        assert!(testing::shows(&mut app, "the name is empty"));
        assert_eq!(testing::count::<SaveArea>(&mut app), 1, "still open");
        let instruction = testing::find::<AreaInstructionField>(&mut app, |_| true);
        assert_eq!(
            app.world().get::<Field>(instruction).unwrap().committed,
            "Check it.",
            "what was typed is kept"
        );
    }

    #[test]
    fn a_custom_area_is_edited_and_deleted() {
        let mut app = harness_app();
        let areas = with_custom(&mut app);
        assert_eq!(testing::count::<EditArea>(&mut app), 1);
        assert_eq!(testing::count::<DeleteArea>(&mut app), 1);
        let edit = testing::find::<EditArea>(&mut app, |e| e.0 == "migrations");
        testing::activate(&mut app, edit);
        testing::settle(&mut app);
        assert!(testing::shows(&mut app, "Edit audit area"));
        let name = testing::find::<AreaNameField>(&mut app, |_| true);
        assert_eq!(
            app.world().get::<Field>(name).unwrap().committed,
            "Migrations"
        );
        let cancel = testing::find::<CancelArea>(&mut app, |_| true);
        testing::activate(&mut app, cancel);
        testing::settle(&mut app);
        assert_eq!(testing::count::<SaveArea>(&mut app), 0);
        assert!(testing::recorded(&mut app).is_empty());
        let delete = testing::find::<DeleteArea>(&mut app, |d| d.0 == "migrations");
        testing::activate(&mut app, delete);
        let left = written_areas(&testing::recorded(&mut app));
        assert_eq!(left, areas[..6]);
    }

    #[test]
    fn typed_text_survives_a_rebuild_for_another_reason() {
        use crate::ui::kit::FieldCommitted;
        let mut app = harness_app();
        let add = testing::find::<AddAreaButton>(&mut app, |_| true);
        testing::activate(&mut app, add);
        testing::settle(&mut app);
        let name = testing::find::<AreaNameField>(&mut app, |_| true);
        app.world_mut().write_message(FieldCommitted {
            entity: name,
            value: "Migrations".into(),
        });
        app.update();
        assert_eq!(app.world().resource::<AreaForm>().name, "Migrations");
        // A probe result or a change made at the terminal rebuilds the page.
        testing::set_config_locally(&mut app, "harness.turn_timeout_secs", "700");
        testing::settle(&mut app);
        let name = testing::find::<AreaNameField>(&mut app, |_| true);
        assert_eq!(
            app.world().get::<Field>(name).unwrap().committed,
            "Migrations"
        );
    }

    #[test]
    fn a_refusal_shows_under_the_list() {
        let mut app = harness_app();
        app.world_mut().resource_mut::<Model>().rejected.insert(
            AREAS_KEY.into(),
            "harness.audit_areas: duplicate id docs".into(),
        );
        testing::settle(&mut app);
        testing::find::<FieldError>(&mut app, |e| e.0 == AREAS_KEY);
        assert!(testing::shows(
            &mut app,
            "harness.audit_areas: duplicate id docs"
        ));
    }

    #[test]
    fn a_built_in_id_is_built_in_whatever_its_flag_says() {
        let mut app = harness_app();
        let mut areas = default_areas();
        areas[5].builtin = false;
        testing::set_config_locally(&mut app, AREAS_KEY, &areas_value(&areas));
        testing::settle(&mut app);
        assert_eq!(testing::count::<AreaSwitch>(&mut app), 6);
        assert_eq!(testing::count::<EditArea>(&mut app), 0);
        assert_eq!(testing::count::<DeleteArea>(&mut app), 0);
        assert_eq!(without(&areas, "docs").len(), 6, "never dropped");
        assert!(saved(&areas, &FormTarget::Edit("docs".into()), "Docs", "Docs.").is_err());
    }

    fn click_switch(app: &mut App, id: &str) -> Vec<AuditArea> {
        let switch = testing::find::<AreaSwitch>(app, |s| s.0 == id);
        testing::activate(app, switch);
        testing::settle(app);
        written_areas(&testing::recorded(app))
    }

    fn enabled(areas: &[AuditArea], id: &str) -> bool {
        areas.iter().find(|a| a.id == id).unwrap().enabled
    }

    #[test]
    fn two_switches_before_the_daemon_answers_keep_both_changes() {
        let mut app = harness_app();
        let first = click_switch(&mut app, "performance");
        assert!(!enabled(&first, "performance"));
        let second = click_switch(&mut app, "docs");
        assert!(
            !enabled(&second, "performance") && !enabled(&second, "docs"),
            "the second write builds on the first"
        );
    }

    #[test]
    fn a_switch_clicked_twice_flips_twice_and_shows_the_change_at_once() {
        let mut app = harness_app();
        let first = click_switch(&mut app, "performance");
        assert!(!enabled(&first, "performance"));
        assert_eq!(
            app.world().resource::<AreaForm>().writing.as_ref(),
            Some(&first),
            "the switches are drawn from the list being written"
        );
        let second = click_switch(&mut app, "performance");
        assert!(enabled(&second, "performance"));
        // The daemon's answer ends the write.
        testing::set_config_locally(&mut app, AREAS_KEY, &areas_value(&second));
        testing::settle(&mut app);
        assert_eq!(app.world().resource::<AreaForm>().writing, None);
    }

    #[test]
    fn a_delete_during_a_switch_keeps_the_switch() {
        let mut app = harness_app();
        with_custom(&mut app);
        click_switch(&mut app, "performance");
        let delete = testing::find::<DeleteArea>(&mut app, |d| d.0 == "migrations");
        testing::activate(&mut app, delete);
        let left = written_areas(&testing::recorded(&mut app));
        assert!(!enabled(&left, "performance"));
        assert!(left.iter().all(|a| a.id != "migrations"));
    }

    #[test]
    fn a_saved_form_closes_when_the_list_arrives() {
        let mut app = harness_app();
        let add = testing::find::<AddAreaButton>(&mut app, |_| true);
        testing::activate(&mut app, add);
        testing::settle(&mut app);
        type_form(
            &mut app,
            "Migrations",
            "Check every migration can be undone.",
        );
        let save = testing::find::<SaveArea>(&mut app, |_| true);
        testing::activate(&mut app, save);
        let written = written_areas(&testing::recorded(&mut app));
        testing::settle(&mut app);
        assert!(app.world().resource::<AreaForm>().saving);
        assert_eq!(
            testing::count::<SaveArea>(&mut app),
            1,
            "open until it is saved"
        );
        testing::set_config_locally(&mut app, AREAS_KEY, &areas_value(&written));
        testing::settle(&mut app);
        assert_eq!(*app.world().resource::<AreaForm>(), AreaForm::default());
        assert_eq!(testing::count::<SaveArea>(&mut app), 0, "the form closed");
    }

    #[test]
    fn a_save_the_daemon_refuses_keeps_the_typed_text() {
        let mut app = harness_app();
        let add = testing::find::<AddAreaButton>(&mut app, |_| true);
        testing::activate(&mut app, add);
        testing::settle(&mut app);
        type_form(
            &mut app,
            "Migrations",
            "Check every migration can be undone.",
        );
        let save = testing::find::<SaveArea>(&mut app, |_| true);
        testing::activate(&mut app, save);
        testing::recorded(&mut app);
        app.world_mut()
            .resource_mut::<Model>()
            .rejected
            .insert(AREAS_KEY.into(), "the daemon is offline".into());
        testing::settle(&mut app);
        let form = app.world().resource::<AreaForm>().clone();
        assert_eq!(form.target, Some(FormTarget::New));
        assert!(!form.saving && form.writing.is_none());
        let name = testing::find::<AreaNameField>(&mut app, |_| true);
        assert_eq!(
            app.world().get::<Field>(name).unwrap().committed,
            "Migrations"
        );
        assert!(testing::shows(&mut app, "the daemon is offline"));
    }

    #[test]
    fn leaving_the_harness_page_closes_the_form() {
        let mut app = harness_app();
        let add = testing::find::<AddAreaButton>(&mut app, |_| true);
        testing::activate(&mut app, add);
        testing::settle(&mut app);
        assert!(app.world().resource::<AreaForm>().target.is_some());
        app.world_mut()
            .resource_mut::<Nav>()
            .open_section(Section::General);
        testing::settle(&mut app);
        app.world_mut()
            .resource_mut::<Nav>()
            .open_section(Section::Harness);
        testing::settle(&mut app);
        assert_eq!(*app.world().resource::<AreaForm>(), AreaForm::default());
        assert_eq!(testing::count::<SaveArea>(&mut app), 0);
    }
}
