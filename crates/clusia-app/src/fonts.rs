//! Text faces. Inter (UI, upright and italic) and JetBrains Mono (code) are OFL and compiled
//! into the binary (spec §7.1). The UI face can be another family instead (the system font or
//! any installed one): its files are read off the main thread, then every text switches to it
//! at once. Code stays JetBrains Mono.

use std::sync::Arc;

use bevy::prelude::*;
use bevy::tasks::futures::check_ready;
use bevy::tasks::{AsyncComputeTaskPool, Task};
use bevy::text::{FontSource, FontStyle};
use clusia_core::config::{INTER as INTER_NAME, SYSTEM_FONT};
use clusia_diagram::Advances;
use skrifa::raw::FileRef;
use skrifa::string::StringId;
use skrifa::{FontRef, MetadataProvider};

use crate::bridge::Model;

pub const INTER: &[u8] = include_bytes!("../assets/fonts/InterVariable.ttf");
pub const INTER_ITALIC: &[u8] = include_bytes!("../assets/fonts/Inter-Italic[opsz,wght].ttf");
pub const JETBRAINS_MONO: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono[wght].ttf");

/// A face as text asks for it: a font source and a slant.
#[derive(Debug, Clone, PartialEq)]
pub struct Face {
    pub source: FontSource,
    pub style: FontStyle,
}

impl Face {
    pub fn handle(handle: &Handle<Font>) -> Self {
        Self {
            source: FontSource::Handle(handle.clone()),
            style: FontStyle::Normal,
        }
    }

    fn family(name: &str, style: FontStyle) -> Self {
        Self {
            source: FontSource::Family(name.into()),
            style,
        }
    }

    pub fn apply(&self, font: &mut TextFont) {
        font.font = self.source.clone();
        font.style = self.style;
    }

    fn is_on(&self, font: &TextFont) -> bool {
        font.font == self.source && font.style == self.style
    }
}

impl Default for Face {
    fn default() -> Self {
        Self::handle(&Handle::default())
    }
}

/// The faces text is drawn in. `Default` (empty handles, Inter's numbers) is for headless
/// tests.
#[derive(Resource, Debug, Clone)]
pub struct UiFonts {
    pub sans: Face,
    /// Emphasis. Inter has no synthetic slant, so it uses its own italic file.
    pub italic: Face,
    pub mono: Handle<Font>,
    /// The vertical metrics of `sans`, for `label_lift`; `None` when its file has none.
    pub metrics: Option<Metrics>,
    /// The advances of `sans`, for the diagrams' text measure.
    pub advances: Arc<Advances>,
}

impl Default for UiFonts {
    fn default() -> Self {
        Self {
            sans: Face::default(),
            italic: Face::default(),
            mono: Handle::default(),
            metrics: inter_metrics(),
            advances: Arc::new(Advances::inter()),
        }
    }
}

/// The bundled faces, added to `fonts`.
pub fn load(fonts: &mut Assets<Font>) -> UiFonts {
    UiFonts {
        sans: Face::handle(&fonts.add(Font::from_bytes(INTER.to_vec()))),
        italic: Face::handle(&fonts.add(Font::from_bytes(INTER_ITALIC.to_vec()))),
        mono: fonts.add(Font::from_bytes(JETBRAINS_MONO.to_vec())),
        ..UiFonts::default()
    }
}

/// Adds the bundled fonts to `Assets<Font>` (which the text plugin, or a test, must have
/// registered), reads installed fonts through Core Text, and follows the configured face.
pub struct FontsPlugin;

impl Plugin for FontsPlugin {
    fn build(&self, app: &mut App) {
        let fonts = load(&mut app.world_mut().resource_mut::<Assets<Font>>());
        app.insert_resource(TextFace::bundled(&fonts))
            .insert_resource(fonts)
            .insert_resource(FontSystem::core_text())
            .add_plugins(FaceFollowPlugin);
    }
}

/// Where faces come from: Core Text in the window; tests put in their own.
#[derive(Resource, Clone, Copy)]
pub struct FontSystem {
    /// The installed families, sorted.
    pub families: fn() -> Vec<String>,
    /// One family read from its files.
    pub load: fn(&str) -> Result<LoadedFace, Unusable>,
}

/// Why a configured family is drawn as Inter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unusable {
    NotInstalled,
    /// Installed, but its files could not be read as fonts.
    Unreadable,
}

impl FontSystem {
    pub fn core_text() -> Self {
        Self {
            families: crate::platform_fonts::installed_families,
            load: load_installed,
        }
    }
}

/// `name` from its files: `SYSTEM_FONT` is the macOS system font.
fn load_installed(name: &str) -> Result<LoadedFace, Unusable> {
    use crate::platform_fonts::FaceFile;
    let family = crate::platform_fonts::family_files(name, name == SYSTEM_FONT)
        .ok_or(Unusable::NotInstalled)?;
    let faces = [
        Some(&family.regular),
        family.bold.as_ref(),
        family.italic.as_ref(),
        family.bold_italic.as_ref(),
    ];
    let mut files: Vec<(&std::path::Path, Vec<u8>)> = Vec::new();
    for face in faces.into_iter().flatten() {
        if files.iter().any(|(path, _)| *path == face.path) {
            continue;
        }
        match std::fs::read(&face.path) {
            Ok(bytes) => files.push((&face.path, bytes)),
            Err(e) => {
                tracing::warn!(path = %face.path.display(), error = %e, "cannot read a font file")
            }
        }
    }
    fn bytes<'a>(
        files: &'a [(&std::path::Path, Vec<u8>)],
        face: &'a FaceFile,
    ) -> Option<(&'a [u8], &'a str)> {
        files
            .iter()
            .find(|(path, _)| *path == face.path)
            .map(|(_, b)| (b.as_slice(), face.postscript.as_str()))
    }
    let bold = family.bold.as_ref().and_then(|f| bytes(&files, f));
    let regular = bytes(&files, &family.regular).ok_or(Unusable::Unreadable)?;
    let mut loaded = LoadedFace::read(regular, bold).ok_or(Unusable::Unreadable)?;
    loaded.files = files.into_iter().map(|(_, b)| b).collect();
    Ok(loaded)
}

/// A family read from its files, ready to draw with.
#[derive(Debug, Clone)]
pub struct LoadedFace {
    /// The family name in the files: what text asks the font collection for.
    pub family: String,
    /// Every file of the family, each once, the regular face's first.
    pub files: Vec<Vec<u8>>,
    pub metrics: Option<Metrics>,
    pub advances: Advances,
}

impl LoadedFace {
    /// The face in `regular` (a file and the PostScript name of the face in it), its advances
    /// at weight 600 taken from `bold` when the family has a bold file. `None` when the file
    /// cannot be read as a font. `files` is left empty: the caller moves the files it read in,
    /// rather than a copy of the regular one.
    pub fn read(regular: (&[u8], &str), bold: Option<(&[u8], &str)>) -> Option<Self> {
        let (index, font) = face_in(regular.0, regular.1)?;
        let family = family_name(&font)?;
        let strong = bold
            .and_then(|(bytes, name)| face_in(bytes, name))
            .map(|(_, f)| f);
        Some(Self {
            family,
            metrics: face_metrics(regular.0, index),
            advances: advances(&font, strong.as_ref()),
            files: Vec::new(),
        })
    }
}

/// The face called `postscript` in `file` (any face when none is), and its index.
/// The index is the face's place in the file, kept past faces that cannot be read; with no
/// name match, it is the first face that reads.
fn face_in<'a>(file: &'a [u8], postscript: &str) -> Option<(u32, FontRef<'a>)> {
    let faces: Vec<(u32, FontRef<'a>)> = match FileRef::new(file).ok()? {
        FileRef::Font(font) => vec![(0, font)],
        FileRef::Collection(c) => (0..)
            .zip(c.iter())
            .filter_map(|(i, face)| Some((i, face.ok()?)))
            .collect(),
    };
    let named = faces.iter().position(|(_, f)| {
        f.localized_strings(StringId::POSTSCRIPT_NAME)
            .english_or_first()
            .is_some_and(|n| n.to_string() == postscript)
    });
    faces.into_iter().nth(named.unwrap_or(0))
}

/// The family name the font collection files `font` under: its typographic family, else its
/// family.
fn family_name(font: &FontRef) -> Option<String> {
    [StringId::TYPOGRAPHIC_FAMILY_NAME, StringId::FAMILY_NAME]
        .into_iter()
        .find_map(|id| font.localized_strings(id).english_or_first())
        .map(|name| name.to_string())
        .filter(|name| !name.is_empty())
}

/// The advance table of `regular`, at weight 600 from `bold` when given, else from the
/// regular face's weight axis (the same file when it has none).
fn advances(regular: &FontRef, bold: Option<&FontRef>) -> Advances {
    let at = |font: &FontRef, weight: f32| -> Vec<Option<f32>> {
        let size = skrifa::instance::Size::unscaled();
        let upem = f32::from(
            font.metrics(size, skrifa::instance::LocationRef::default())
                .units_per_em
                .max(1),
        );
        let location = font.axes().location([("wght", weight)]);
        let metrics = font.glyph_metrics(size, &location);
        let charmap = font.charmap();
        Advances::sampled()
            .map(|c| {
                let glyph = charmap.map(c)?;
                metrics.advance_width(glyph).map(|w| w / upem)
            })
            .collect()
    };
    let regular_em = at(regular, 400.0);
    let strong_em = at(bold.unwrap_or(regular), 600.0);
    let index = |c: char| Advances::sampled().position(|s| s == c);
    Advances::from_face(|c, strong| {
        let i = index(c)?;
        if strong { strong_em[i] } else { regular_em[i] }
    })
}

/// The face the config asks for, as it is drawn: the one in use, a read under way, and a
/// family this Mac does not have.
#[derive(Resource, Default)]
pub struct TextFace {
    /// The configured name drawn now: `INTER` until another is read, and while one is missing.
    pub applied: String,
    /// A family the config names that cannot be used, and why; text shows in Inter meanwhile.
    pub missing: Option<(String, Unusable)>,
    pending: Option<(String, Task<Result<LoadedFace, Unusable>>)>,
    /// The bundled faces, to go back to.
    bundled: (Face, Face),
    /// The fonts of the face in use, kept alive while it is.
    held: Vec<Handle<Font>>,
}

impl TextFace {
    pub fn bundled(fonts: &UiFonts) -> Self {
        Self {
            applied: INTER_NAME.into(),
            bundled: (fonts.sans.clone(), fonts.italic.clone()),
            ..Self::default()
        }
    }

    /// A family is being read.
    pub fn reading(&self) -> Option<&str> {
        self.pending.as_ref().map(|(name, _)| name.as_str())
    }
}

/// The installed families, for Config's list: `None` until they have been read.
#[derive(Resource, Default)]
pub struct InstalledFonts {
    pub families: Option<Vec<String>>,
    task: Option<Task<Vec<String>>>,
}

/// Follows `appearance.text_font`. Without a `FontSystem` (headless tests) it does nothing.
pub struct FaceFollowPlugin;

impl Plugin for FaceFollowPlugin {
    fn build(&self, app: &mut App) {
        if !app.world().contains_resource::<TextFace>() {
            let face = TextFace::bundled(&app.world().resource::<UiFonts>().clone());
            app.insert_resource(face);
        }
        app.init_resource::<InstalledFonts>()
            .add_systems(Startup, read_installed)
            .add_systems(Update, list_installed.run_if(reading_installed))
            // Before the text plugin files new fonts, so text asks for a family it already has.
            .add_systems(
                PostUpdate,
                follow_text_font.before(bevy::text::load_font_assets_into_font_collection),
            );
    }
}

fn read_installed(system: Option<Res<FontSystem>>, mut installed: ResMut<InstalledFonts>) {
    if let Some(system) = system {
        let families = system.families;
        installed.task = Some(AsyncComputeTaskPool::get().spawn(async move { families() }));
    }
}

fn reading_installed(installed: Res<InstalledFonts>) -> bool {
    installed.task.is_some()
}

fn list_installed(mut installed: ResMut<InstalledFonts>) {
    if let Some(task) = installed.task.as_mut()
        && let Some(families) = check_ready(task)
    {
        installed.families = Some(families);
        installed.task = None;
    }
}

/// Starts reading the configured family when it changes and swaps it in once read; a family
/// that is not installed leaves Inter and is named in `TextFace::missing`.
fn follow_text_font(
    model: Res<Model>,
    system: Option<Res<FontSystem>>,
    mut face: ResMut<TextFace>,
    mut fonts: ResMut<UiFonts>,
    mut assets: Option<ResMut<Assets<Font>>>,
    mut texts: Query<&mut TextFont>,
) {
    let Some(system) = system else { return };
    let wanted = model.snapshot.config.appearance.text_font.as_str();
    let mut finished = false;
    // Polling needs the task mutably, which must not mark the face changed on every frame.
    let read = face
        .bypass_change_detection()
        .pending
        .as_mut()
        .and_then(|(_, task)| check_ready(task));
    if let Some(read) = read
        && let Some((name, _)) = face.pending.take()
    {
        finished = true;
        if name == wanted {
            let next = match read {
                Ok(read) => {
                    face.missing = None;
                    face.applied = name;
                    let held: Vec<Handle<Font>> = match assets.as_mut() {
                        Some(assets) => read
                            .files
                            .into_iter()
                            .map(|bytes| assets.add(Font::from_bytes(bytes)))
                            .collect(),
                        None => Vec::new(),
                    };
                    face.held = held;
                    UiFonts {
                        sans: Face::family(&read.family, FontStyle::Normal),
                        italic: Face::family(&read.family, FontStyle::Italic),
                        mono: fonts.mono.clone(),
                        metrics: read.metrics,
                        advances: Arc::new(read.advances),
                    }
                }
                Err(why) => {
                    tracing::warn!(family = %name, ?why, "the text font cannot be used; using Inter");
                    face.missing = Some((name, why));
                    bundled(&mut face, &fonts)
                }
            };
            swap(&mut fonts, next, &mut texts);
        }
    }
    if !(model.is_changed() || system.is_added() || finished) {
        return;
    }
    if wanted == INTER_NAME {
        face.pending = None;
        face.missing = None;
        if face.applied != INTER_NAME {
            let next = bundled(&mut face, &fonts);
            swap(&mut fonts, next, &mut texts);
        }
        return;
    }
    let settled = face.applied == wanted
        || face
            .missing
            .as_ref()
            .is_some_and(|(name, _)| name == wanted)
        || face.reading() == Some(wanted);
    if !settled {
        let (load, name) = (system.load, wanted.to_string());
        let task = AsyncComputeTaskPool::get().spawn(async move { load(&name) });
        face.pending = Some((wanted.to_string(), task));
    }
}

/// The bundled faces, and `face` back on them.
fn bundled(face: &mut TextFace, fonts: &UiFonts) -> UiFonts {
    face.applied = INTER_NAME.into();
    face.held.clear();
    UiFonts {
        sans: face.bundled.0.clone(),
        italic: face.bundled.1.clone(),
        mono: fonts.mono.clone(),
        ..UiFonts::default()
    }
}

/// Puts `next` in place of `fonts` on every text drawn in the old faces. Only texts that
/// change are touched; the text plugin lays those out again.
fn swap(fonts: &mut UiFonts, next: UiFonts, texts: &mut Query<&mut TextFont>) {
    for mut font in texts.iter_mut() {
        if fonts.sans.is_on(&font) {
            next.sans.apply(&mut font);
        } else if fonts.italic.is_on(&font) {
            next.italic.apply(&mut font);
        }
    }
    *fonts = next;
}

/// Vertical metrics of a font, in its own units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    pub units_per_em: f32,
    pub ascent: f32,
    /// Positive, below the baseline.
    pub descent: f32,
    pub x_height: f32,
    pub cap_height: f32,
}

/// The `hhea` line metrics (what the text layout puts a line box on) and the `OS/2` x-height and
/// cap height of `font`; `None` when a table is missing or too short.
pub(crate) fn metrics(font: &[u8]) -> Option<Metrics> {
    face_metrics(font, 0)
}

/// `metrics` of face `index` of `file`, a font or a font collection (whose table offsets are
/// from the start of the file too).
fn face_metrics(file: &[u8], index: u32) -> Option<Metrics> {
    let u16_at = |at: usize| Some(u16::from_be_bytes(file.get(at..at + 2)?.try_into().ok()?));
    let u32_at = |at: usize| Some(u32::from_be_bytes(file.get(at..at + 4)?.try_into().ok()?));
    let i16_at = |at: usize| u16_at(at).map(|v| f32::from(v as i16));
    let dir = if file.get(..4) == Some(b"ttcf") {
        u32_at(12 + 4 * index as usize)? as usize
    } else {
        0
    };
    let table = |tag: &[u8; 4]| {
        (0..u16_at(dir + 4)? as usize)
            .map(|i| dir + 12 + 16 * i)
            .find(|&r| file.get(r..r + 4) == Some(&tag[..]))
            .and_then(|r| Some(u32_at(r + 8)? as usize))
    };
    let (head, hhea, os2) = (table(b"head")?, table(b"hhea")?, table(b"OS/2")?);
    Some(Metrics {
        units_per_em: f32::from(u16_at(head + 18)?),
        ascent: i16_at(hhea + 4)?,
        descent: -i16_at(hhea + 6)?,
        x_height: i16_at(os2 + 86)?,
        cap_height: i16_at(os2 + 88)?,
    })
}

fn inter_metrics() -> Option<Metrics> {
    static INTER_METRICS: std::sync::OnceLock<Option<Metrics>> = std::sync::OnceLock::new();
    *INTER_METRICS.get_or_init(|| metrics(INTER))
}

/// How far, in points, a one-line label at `size` has to be raised so the middle of its
/// letters (half way between the x-height and the cap height) sits on the middle of its box.
/// The line box is centred by layout, but a font's ascent is usually larger than its descent,
/// which leaves the letters low in it. No metrics, no lift.
pub fn lift(metrics: Option<&Metrics>, size: f32) -> f32 {
    let Some(m) = metrics else { return 0.0 };
    let letters_middle = m.ascent - (m.x_height + m.cap_height) / 4.0;
    let box_middle = (m.ascent + m.descent) / 2.0;
    (letters_middle - box_middle) / m.units_per_em * size
}

/// `lift` for Inter.
pub fn label_lift(size: f32) -> f32 {
    lift(inter_metrics().as_ref(), size)
}

/// Whether `font` (TrueType bytes) has a glyph for `ch`, read from its Unicode `cmap`
/// subtable (format 12, else format 4). Without one, the text shows a missing-glyph box.
#[cfg(test)]
pub(crate) fn covers(font: &[u8], ch: char) -> bool {
    let u16_at = |at: usize| u16::from_be_bytes([font[at], font[at + 1]]) as usize;
    let u32_at = |at: usize| u32::from_be_bytes(font[at..at + 4].try_into().unwrap()) as usize;
    let Some(cmap) = (0..u16_at(4))
        .map(|i| 12 + 16 * i)
        .find(|&r| &font[r..r + 4] == b"cmap")
        .map(|r| u32_at(r + 8))
    else {
        return false;
    };
    let subtables: Vec<usize> = (0..u16_at(cmap + 2))
        .map(|i| cmap + u32_at(cmap + 4 + 8 * i + 4))
        .collect();
    let code = ch as usize;
    if let Some(&t) = subtables.iter().find(|&&t| u16_at(t) == 12) {
        return (0..u32_at(t + 12)).any(|g| {
            let group = t + 16 + 12 * g;
            let (start, end, glyph) = (u32_at(group), u32_at(group + 4), u32_at(group + 8));
            (start..=end).contains(&code) && glyph + code - start != 0
        });
    }
    let Some(&t) = subtables.iter().find(|&&t| u16_at(t) == 4) else {
        return false;
    };
    let segments = u16_at(t + 6) / 2;
    let ends = t + 14;
    let starts = ends + 2 * segments + 2;
    let deltas = starts + 2 * segments;
    let ranges = deltas + 2 * segments;
    (0..segments).any(|i| {
        let (start, end) = (u16_at(starts + 2 * i), u16_at(ends + 2 * i));
        if !(start..=end).contains(&code) {
            return false;
        }
        let range = u16_at(ranges + 2 * i);
        let glyph = if range == 0 {
            code
        } else {
            u16_at(ranges + 2 * i + range + 2 * (code - start))
        };
        glyph != 0 && (glyph + u16_at(deltas + 2 * i)) % 0x10000 != 0
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::Snapshot;
    use crate::testing;
    use bevy::ecs::system::RunSystemOnce;
    use bevy::text::FontSize;
    use clusia_diagram::{FaceMeasure, Measure, TextStyle};

    #[test]
    fn covers_reads_the_character_map() {
        for font in [INTER, INTER_ITALIC, JETBRAINS_MONO] {
            assert!(covers(font, 'A') && covers(font, '±'));
            assert!(!covers(font, '\u{2623}'), "no biohazard sign");
        }
        assert!(covers(JETBRAINS_MONO, '◎') && !covers(INTER, '◎'));
        assert!(!covers(INTER, '☺') && !covers(INTER, '▣'));
    }

    #[test]
    fn inter_metrics_are_read_from_the_font_file() {
        let m = metrics(INTER).unwrap();
        assert_eq!(m.units_per_em, 2048.0);
        assert!(m.ascent > 1900.0 && m.ascent < 2100.0, "{m:?}");
        assert!(m.descent > 400.0 && m.descent < 600.0, "{m:?}");
        assert!(m.x_height > 1000.0 && m.cap_height > m.x_height, "{m:?}");
    }

    #[test]
    fn a_label_is_lifted_so_its_letters_sit_on_the_middle_of_the_line() {
        let m = metrics(INTER).unwrap();
        // The line box runs from the ascent to the descent; the letters' middle (half way up
        // between the x-height and the cap height) must land on the box's middle.
        let size = 13.0;
        let lift = label_lift(size);
        let box_middle = (m.ascent + m.descent) / 2.0;
        let letters_middle = m.ascent - (m.x_height + m.cap_height) / 4.0;
        let want = (letters_middle - box_middle) / m.units_per_em * size;
        assert!((lift - want).abs() < 0.01, "lift {lift}, wanted {want}");
        assert!(
            lift > 0.2 && lift < 1.0,
            "a sub-pixel to one pixel nudge: {lift}"
        );
        assert_eq!(label_lift(26.0), lift * 2.0, "scales with the size");
    }

    #[test]
    fn fonts_are_embedded_and_distinct() {
        assert!(INTER.len() > 500_000 && JETBRAINS_MONO.len() > 200_000);
        assert!(INTER_ITALIC.len() > 500_000);
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, bevy::asset::AssetPlugin::default()))
            .init_asset::<Font>()
            .add_plugins(FontsPlugin);
        let fonts = app.world().resource::<UiFonts>().clone();
        let handle = |face: &Face| match &face.source {
            FontSource::Handle(h) => h.clone(),
            other => panic!("not bundled: {other:?}"),
        };
        let (sans, italic) = (handle(&fonts.sans), handle(&fonts.italic));
        assert_ne!(sans, fonts.mono);
        assert_ne!(sans, italic);
        assert_ne!(italic, fonts.mono);
        let assets = app.world().resource::<Assets<Font>>();
        assert!(assets.get(&sans).is_some() && assets.get(&fonts.mono).is_some());
        assert!(assets.get(&italic).is_some());
    }

    #[test]
    fn a_family_is_read_from_its_file_with_its_own_metrics_and_advances() {
        // The name must be the one the text plugin's font collection files the face under.
        let filed = |bytes: &[u8]| {
            let mut cx = bevy::text::FontCx::default();
            let family = cx
                .collection
                .register_fonts(Font::from_bytes(bytes.to_vec()).data, None)[0]
                .0;
            cx.collection.family_name(family).unwrap().to_string()
        };
        let inter = LoadedFace::read((INTER, ""), None).unwrap();
        assert_eq!(inter.family, "Inter Variable");
        assert_eq!(inter.family, filed(INTER));
        assert_eq!(inter.metrics, metrics(INTER));
        assert!(
            inter.files.is_empty(),
            "the caller files its bytes, not a copy"
        );
        let mono = LoadedFace::read((JETBRAINS_MONO, "JetBrainsMono-Regular"), None).unwrap();
        assert_eq!(mono.family, "JetBrains Mono");
        assert_eq!(mono.family, filed(JETBRAINS_MONO));
        assert_ne!(mono.metrics, inter.metrics);
        assert!(LoadedFace::read((b"not a font", ""), None).is_none());
    }

    #[test]
    fn the_diagram_measure_matches_the_chosen_font() {
        // Inter read from its file measures as the built-in tables, which were taken from it.
        let read = LoadedFace::read((INTER, ""), None).unwrap().advances;
        let built_in = FaceMeasure::new(Arc::new(Advances::inter()), 1.0);
        let from_file = FaceMeasure::new(Arc::new(read), 1.0);
        for style in [TextStyle::Title, TextStyle::Row, TextStyle::SubStrong] {
            for text in ["TokenStore::refresh", "Refresh call → step · 3", "OAuth"] {
                let (a, b) = (built_in.width(text, style), from_file.width(text, style));
                assert!((a - b).abs() <= a * 0.01, "{text:?} {style:?}: {a} vs {b}");
            }
        }
        // Another face measures as itself: JetBrains Mono is 0.6 em a character.
        let mono = LoadedFace::read((JETBRAINS_MONO, ""), None)
            .unwrap()
            .advances;
        let m = FaceMeasure::new(Arc::new(mono), 15.0 / 13.0);
        let (px, _, _) = clusia_diagram::style_size(TextStyle::Row);
        let want = 6.0 * 0.6 * px * 15.0 / 13.0;
        let got = m.width("Commit", TextStyle::Row);
        assert!((got / want - 1.02).abs() < 0.01, "{got} vs {want}");
    }

    #[test]
    fn metrics_of_a_collection_face_follow_its_offset() {
        assert_eq!(face_metrics(INTER, 0), metrics(INTER));
        let mut ttc = b"ttcf\x00\x01\x00\x00\x00\x00\x00\x01\x00\x00\x00\x10".to_vec();
        ttc.extend_from_slice(&[0; 12]);
        assert_eq!(face_metrics(&ttc, 0), None, "no tables at the offset");
        assert_eq!(face_metrics(&ttc, 3), None, "no such face");
    }

    #[test]
    fn the_lift_follows_the_face() {
        let mono = metrics(JETBRAINS_MONO).unwrap();
        assert_eq!(lift(None, 13.0), 0.0);
        assert_ne!(lift(Some(&mono), 13.0), label_lift(13.0));
        assert_eq!(lift(inter_metrics().as_ref(), 13.0), label_lift(13.0));
    }

    /// Reads this Mac's own fonts; run by hand (`--ignored`), as the families differ by Mac.
    #[test]
    #[ignore]
    fn core_text_finds_the_system_font_and_installed_families() {
        let system = FontSystem::core_text();
        let families = (system.families)();
        assert!(
            families.iter().any(|f| f == "Helvetica Neue"),
            "{families:?}"
        );
        assert!(families.iter().all(|f| !f.starts_with('.')));
        let sf = (system.load)(SYSTEM_FONT).expect("the system font");
        assert!(
            sf.metrics.is_some() && !sf.files.is_empty(),
            "{}",
            sf.family
        );
        let helvetica = (system.load)("Helvetica Neue").expect("Helvetica Neue");
        assert_eq!(helvetica.family, "Helvetica Neue");
        assert_ne!(helvetica.advances, Advances::inter());
        assert_eq!(
            (system.load)("Nope Sans").err(),
            Some(Unusable::NotInstalled)
        );
        eprintln!("system font: {} in {} file(s)", sf.family, sf.files.len());
    }

    /// `bytes` read as a family of one file.
    fn one_file(bytes: &[u8]) -> Result<LoadedFace, Unusable> {
        let mut face = LoadedFace::read((bytes, ""), None).ok_or(Unusable::Unreadable)?;
        face.files.push(bytes.to_vec());
        Ok(face)
    }

    /// A Mac with Fira Code (the bundled mono stands in for it), an Iosevka that takes a while
    /// to read, a copy of Inter, and a Broken Sans whose file is not a font.
    fn fake_system() -> FontSystem {
        FontSystem {
            families: || vec!["Fira Code".into(), "Inter".into()],
            load: |name| match name {
                "Fira Code" => one_file(JETBRAINS_MONO),
                "Iosevka" => {
                    std::thread::sleep(std::time::Duration::from_millis(150));
                    one_file(JETBRAINS_MONO)
                }
                "Inter Copy" => one_file(INTER),
                "Broken Sans" => one_file(b"not a font"),
                _ => Err(Unusable::NotInstalled),
            },
        }
    }

    #[test]
    fn a_family_chosen_while_another_is_read_wins() {
        let mut app = face_app(INTER_NAME);
        testing::set_config_locally(&mut app, "appearance.text_font", "Iosevka");
        assert_eq!(
            app.world().resource::<TextFace>().reading(),
            Some("Iosevka")
        );
        testing::set_config_locally(&mut app, "appearance.text_font", "Inter Copy");
        settle(&mut app);
        // Long enough for the first read to have finished, had it been kept.
        std::thread::sleep(std::time::Duration::from_millis(200));
        for _ in 0..3 {
            app.update();
        }
        assert_eq!(app.world().resource::<TextFace>().applied, "Inter Copy");
        assert_eq!(
            app.world().resource::<UiFonts>().sans,
            Face::family("Inter Variable", FontStyle::Normal),
            "the earlier read never lands"
        );
    }

    #[test]
    fn a_family_that_cannot_be_read_says_so() {
        let mut app = face_app("Broken Sans");
        settle(&mut app);
        assert_eq!(
            app.world().resource::<TextFace>().missing,
            Some(("Broken Sans".into(), Unusable::Unreadable))
        );
    }

    #[derive(Resource, Default)]
    struct Marks(usize);

    #[test]
    fn the_face_is_marked_changed_only_when_it_changes() {
        let mut app = face_app(INTER_NAME);
        app.init_resource::<Marks>().add_systems(
            Last,
            |face: Res<TextFace>, mut marks: ResMut<Marks>| {
                if face.is_changed() {
                    marks.0 += 1;
                }
            },
        );
        for _ in 0..3 {
            app.update();
        }
        app.world_mut().resource_mut::<Marks>().0 = 0;
        for _ in 0..5 {
            app.update();
        }
        assert_eq!(
            app.world().resource::<Marks>().0,
            0,
            "idle frames mark nothing"
        );
        testing::set_config_locally(&mut app, "appearance.text_font", "Fira Code");
        settle(&mut app);
        assert!(app.world().resource::<Marks>().0 > 0, "a read does");
    }

    /// `font` (a single font file) as face 1 of a collection whose face 0 cannot be read.
    fn second_of_a_collection(font: &[u8]) -> Vec<u8> {
        let header: u32 = 12 + 4 * 2;
        let mut ttc = b"ttcf\x00\x01\x00\x00".to_vec();
        ttc.extend_from_slice(&2u32.to_be_bytes());
        ttc.extend_from_slice(&u32::MAX.to_be_bytes());
        ttc.extend_from_slice(&header.to_be_bytes());
        let mut body = font.to_vec();
        let tables = usize::from(u16::from_be_bytes([font[4], font[5]]));
        for i in 0..tables {
            let at = 12 + 16 * i + 8;
            let offset = u32::from_be_bytes(font[at..at + 4].try_into().unwrap());
            body[at..at + 4].copy_from_slice(&(offset + header).to_be_bytes());
        }
        ttc.extend_from_slice(&body);
        ttc
    }

    #[test]
    fn a_face_of_a_collection_keeps_its_index_when_another_face_is_broken() {
        let ttc = second_of_a_collection(INTER);
        let postscript = FontRef::new(INTER)
            .unwrap()
            .localized_strings(StringId::POSTSCRIPT_NAME)
            .english_or_first()
            .unwrap()
            .to_string();
        let (index, _) = face_in(&ttc, &postscript).unwrap();
        assert_eq!(index, 1, "its place in the file");
        assert_eq!(face_metrics(&ttc, index), metrics(INTER));
        let face = LoadedFace::read((&ttc, &postscript), None).unwrap();
        assert_eq!(face.metrics, metrics(INTER));
        let (index, _) = face_in(&ttc, "NoSuchFace").unwrap();
        assert_eq!(index, 1, "no name match: the first face that reads");
    }

    fn face_app(text_font: &str) -> App {
        let mut snap = Snapshot::default();
        snap.config.appearance.text_font = text_font.into();
        let mut app = testing::app(snap);
        app.init_asset::<Font>();
        let fonts = load(&mut app.world_mut().resource_mut::<Assets<Font>>());
        app.insert_resource(TextFace::bundled(&fonts))
            .insert_resource(fonts)
            .insert_resource(fake_system());
        app
    }

    /// Updates until no family is being read.
    fn settle(app: &mut App) {
        for _ in 0..500 {
            app.update();
            if app.world().resource::<TextFace>().reading().is_none() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("the family was never read");
    }

    #[test]
    fn choosing_a_family_switches_every_text_at_once_and_back() {
        let mut app = face_app(INTER_NAME);
        let inter = app.world().resource::<UiFonts>().clone();
        let t = crate::ui::kit::Type::BODY;
        let body = app
            .world_mut()
            .spawn(crate::ui::kit::text(&inter, "a", t))
            .id();
        let mut em = crate::ui::kit::font(&inter, t);
        inter.italic.apply(&mut em);
        let italic = app.world_mut().spawn((Text::new("b"), em)).id();
        let code = crate::ui::kit::Type::MONO;
        let mono = app
            .world_mut()
            .spawn(crate::ui::kit::text(&inter, "c", code))
            .id();
        app.update();
        let font = |app: &App, e: Entity| app.world().get::<TextFont>(e).unwrap().clone();

        testing::set_config_locally(&mut app, "appearance.text_font", "Fira Code");
        settle(&mut app);
        let fonts = app.world().resource::<UiFonts>().clone();
        assert_eq!(
            fonts.sans,
            Face::family("JetBrains Mono", FontStyle::Normal)
        );
        assert_eq!(
            fonts.italic,
            Face::family("JetBrains Mono", FontStyle::Italic)
        );
        assert!(
            fonts.sans.is_on(&font(&app, body)),
            "the text already on screen"
        );
        assert!(fonts.italic.is_on(&font(&app, italic)));
        assert_eq!(
            font(&app, mono).font,
            FontSource::Handle(inter.mono.clone())
        );
        assert_eq!(
            font(&app, body).font_size,
            FontSize::Rem(13.0),
            "only the face"
        );
        assert_eq!(fonts.metrics, metrics(JETBRAINS_MONO));
        assert_ne!(*fonts.advances, Advances::inter());
        let face = app.world().resource::<TextFace>();
        assert_eq!(
            (face.applied.as_str(), face.missing.as_ref()),
            ("Fira Code", None)
        );
        assert_eq!(face.held.len(), 1, "its file is a font asset");

        testing::set_config_locally(&mut app, "appearance.text_font", INTER_NAME);
        let fonts = app.world().resource::<UiFonts>().clone();
        assert_eq!(fonts.sans, inter.sans, "Inter needs no reading");
        assert!(inter.sans.is_on(&font(&app, body)));
        assert!(inter.italic.is_on(&font(&app, italic)));
        assert_eq!(*fonts.advances, Advances::inter());
        assert!(app.world().resource::<TextFace>().held.is_empty());
    }

    #[test]
    fn a_family_that_is_not_installed_at_start_shows_as_inter_and_is_named() {
        let mut app = face_app("Nope Sans");
        let inter = app.world().resource::<UiFonts>().sans.clone();
        settle(&mut app);
        let face = app.world().resource::<TextFace>();
        assert_eq!(
            face.missing,
            Some(("Nope Sans".into(), Unusable::NotInstalled))
        );
        assert_eq!(face.applied, INTER_NAME);
        assert_eq!(app.world().resource::<UiFonts>().sans, inter);
        // Asked again, it is not read again; another choice clears it.
        app.update();
        assert!(app.world().resource::<TextFace>().reading().is_none());
        testing::set_config_locally(&mut app, "appearance.text_font", "Fira Code");
        settle(&mut app);
        assert_eq!(app.world().resource::<TextFace>().missing, None);
    }

    #[test]
    fn the_installed_families_are_read_off_the_main_thread() {
        let mut app = face_app(INTER_NAME);
        app.world_mut().run_system_once(read_installed).unwrap();
        for _ in 0..500 {
            app.update();
            if app.world().resource::<InstalledFonts>().families.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(
            app.world().resource::<InstalledFonts>().families,
            Some(vec!["Fira Code".to_string(), "Inter".to_string()])
        );
    }
}
