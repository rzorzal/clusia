//! Pictures in comments: the cache of fetched images and GIFs, the systems that fill the
//! `MdImage` slots, and the clock that keeps the window awake only while a GIF is moving.
//!
//! The window reads only files whose path the daemon returned; a missing file is fetched again
//! once, then reported.

use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::time::Duration;

use bevy::asset::RenderAssetUsages;
use bevy::prelude::*;
use bevy::ui::{CalculatedClip, ComputedNode, UiGlobalTransform, UiSystems};
use bevy::window::PrimaryWindow;
use bevy::winit::{UpdateMode, WinitSettings};
use clusia_core::media::MediaKind;
use clusia_protocol::MediaFile;
use image::{
    AnimationDecoder, DynamicImage, ImageDecoder, ImageError, ImageFormat, ImageReader, Limits,
};

use crate::bridge::{Ask, Asks, MediaError};
use crate::fonts::UiFonts;
use crate::ui::kit::{Type, text};
use crate::ui::markdown::{MdImage, link_chip};

/// Frames kept from one GIF.
pub const MAX_GIF_FRAMES: usize = 300;
/// The longest side, in pixels, of a picture the window decodes.
pub const MAX_SIDE: u32 = 4096;
/// Frames stop being kept once a GIF holds this many pixels in total.
const MAX_GIF_PIXELS: u64 = 48_000_000;
/// Browsers show GIF frames that ask for less than this for this long instead.
const MIN_FRAME: Duration = Duration::from_millis(20);
const DEFAULT_FRAME: Duration = Duration::from_millis(100);

/// The frames of an animated GIF, each a full picture.
#[derive(Debug, Clone, PartialEq)]
pub struct GifFrames {
    pub frames: Vec<Handle<Image>>,
    pub delays: Vec<Duration>,
    pub size: UVec2,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MediaState {
    Loading,
    Image(Handle<Image>),
    Gif(GifFrames),
    Refused(String),
    Failed(String),
}

/// What the daemon fetched for each URL, decoded. One entry per URL, so a picture used twice is
/// asked for once.
#[derive(Resource, Debug, Default)]
pub struct MediaCache {
    pub by_url: HashMap<String, MediaState>,
    /// Answers waiting to be decoded.
    arrived: Vec<(String, Result<MediaFile, MediaError>)>,
    /// URLs whose cached file was gone and have been asked for again.
    retried: HashSet<String>,
}

impl MediaCache {
    /// Queues the daemon's answer for `url`.
    pub fn arrive(&mut self, url: String, file: Result<MediaFile, MediaError>) {
        self.arrived.push((url, file));
    }

    /// Forgets the pictures that failed for a reason that may have passed (the connection, the
    /// daemon's network), so their slots ask again. Refused pictures stay cached: asking again
    /// would get the same answer.
    pub fn retry_failed(&mut self) {
        self.by_url
            .retain(|_, state| !matches!(state, MediaState::Failed(_)));
        self.retried.clear();
    }
}

/// What a slot currently shows.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
enum Showing {
    Loading,
    Image,
    Gif,
    Refused(String),
    Failed(String),
}

/// An animated picture. `visible` is kept up to date by `track_visibility`.
#[derive(Component, Debug, Clone, PartialEq)]
pub struct GifAnim {
    pub frames: Vec<Handle<Image>>,
    pub delays: Vec<Duration>,
    pub i: usize,
    pub t: Duration,
    pub visible: bool,
}

impl GifAnim {
    pub fn new(gif: &GifFrames) -> Self {
        Self {
            frames: gif.frames.clone(),
            delays: gif.delays.clone(),
            i: 0,
            t: Duration::ZERO,
            visible: false,
        }
    }

    /// Whether it has more than one frame to show.
    pub fn moves(&self) -> bool {
        self.frames.len() > 1
    }

    /// Moves `dt` forward; `true` when the frame changed.
    pub fn advance(&mut self, dt: Duration) -> bool {
        if !self.moves() {
            return false;
        }
        self.t += dt;
        let start = self.i;
        // A long pause (a closed laptop) must not spin through the loop for ages.
        for _ in 0..self.frames.len() {
            let delay = self.delays[self.i];
            if self.t < delay {
                return self.i != start;
            }
            self.t -= delay;
            self.i = (self.i + 1) % self.frames.len();
        }
        self.t = Duration::ZERO;
        self.i != start
    }

    /// The shortest delay of its frames.
    pub fn shortest(&self) -> Duration {
        self.delays.iter().copied().min().unwrap_or(DEFAULT_FRAME)
    }
}

/// Whether a node counts as on screen: shown, not empty, and inside both the window and the
/// clip of its scroll areas. Sizes and positions are physical pixels.
pub fn on_screen(
    shown: bool,
    centre: Vec2,
    size: Vec2,
    clip: Option<Rect>,
    window: Option<Vec2>,
) -> bool {
    if !shown || size.x <= 0.0 || size.y <= 0.0 {
        return false;
    }
    let rect = Rect::from_center_size(centre, size);
    if let Some(w) = window
        && rect.intersect(Rect::new(0.0, 0.0, w.x, w.y)).is_empty()
    {
        return false;
    }
    if let Some(c) = clip
        && rect.intersect(c).is_empty()
    {
        return false;
    }
    true
}

/// How long a GIF frame lasts on screen.
fn frame_delay(asked: Duration) -> Duration {
    if asked < MIN_FRAME {
        DEFAULT_FRAME
    } else {
        asked
    }
}

fn decode(file: &MediaFile, images: &mut Assets<Image>) -> Result<MediaState, std::io::Error> {
    let bytes = std::fs::read(&file.path)?;
    let state = match file.kind {
        MediaKind::Gif => decode_gif(&bytes, images),
        MediaKind::Png => decode_still(&bytes, ImageFormat::Png, images),
        MediaKind::Jpeg => decode_still(&bytes, ImageFormat::Jpeg, images),
    };
    Ok(state.unwrap_or_else(MediaState::Refused))
}

fn decode_still(
    bytes: &[u8],
    format: ImageFormat,
    images: &mut Assets<Image>,
) -> Result<MediaState, String> {
    // The limits make the decoder refuse from the header, before it allocates the pixels.
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    reader.limits(limits);
    let picture = reader.decode().map_err(|e| match e {
        ImageError::Limits(_) => "The picture is too large to show".to_string(),
        _ => "The picture is damaged".to_string(),
    })?;
    // Only the GPU needs the pixels once uploaded.
    Ok(MediaState::Image(images.add(Image::from_dynamic(
        picture,
        true,
        RenderAssetUsages::RENDER_WORLD,
    ))))
}

fn decode_gif(bytes: &[u8], images: &mut Assets<Image>) -> Result<MediaState, String> {
    let damaged = || "The GIF is damaged".to_string();
    let decoder = image::codecs::gif::GifDecoder::new(Cursor::new(bytes)).map_err(|_| damaged())?;
    let (width, height) = decoder.dimensions();
    if width > MAX_SIDE || height > MAX_SIDE {
        return Err("The GIF is too large to show".into());
    }
    let mut frames = Vec::new();
    let mut delays = Vec::new();
    for frame in decoder.into_frames().take(MAX_GIF_FRAMES) {
        let Ok(frame) = frame else { break };
        // Keeps at least one frame, then stops before the total passes the cap.
        let total = (frames.len() as u64 + 1) * u64::from(width) * u64::from(height);
        if !frames.is_empty() && total > MAX_GIF_PIXELS {
            break;
        }
        delays.push(frame_delay(Duration::from(frame.delay())));
        frames.push(images.add(Image::from_dynamic(
            DynamicImage::ImageRgba8(frame.into_buffer()),
            true,
            RenderAssetUsages::RENDER_WORLD,
        )));
    }
    if frames.is_empty() {
        return Err(damaged());
    }
    Ok(MediaState::Gif(GifFrames {
        frames,
        delays,
        size: UVec2::new(width, height),
    }))
}

fn decode_arrived(
    mut cache: ResMut<MediaCache>,
    mut images: ResMut<Assets<Image>>,
    mut asks: ResMut<Asks>,
) {
    for (url, answer) in std::mem::take(&mut cache.arrived) {
        let state = match answer {
            Err(e) if e.transient => MediaState::Failed(e.message),
            Err(e) => MediaState::Refused(e.message),
            Ok(file) => match decode(&file, &mut images) {
                Ok(state) => state,
                Err(e)
                    if e.kind() == std::io::ErrorKind::NotFound
                        && cache.retried.insert(url.clone()) =>
                {
                    asks.send(Ask::FetchMedia(url.clone()));
                    MediaState::Loading
                }
                Err(_) => MediaState::Refused("The picture cannot be read".into()),
            },
        };
        cache.by_url.insert(url, state);
    }
}

/// Asks for every picture a slot wants and shows what the cache holds.
fn fill_slots(
    mut commands: Commands,
    fonts: Res<UiFonts>,
    images: Res<Assets<Image>>,
    mut cache: ResMut<MediaCache>,
    mut asks: ResMut<Asks>,
    slots: Query<(Entity, &MdImage, Option<&Showing>)>,
) {
    for (entity, slot, showing) in &slots {
        if !cache.by_url.contains_key(&slot.0) {
            cache.by_url.insert(slot.0.clone(), MediaState::Loading);
            asks.send(Ask::FetchMedia(slot.0.clone()));
        }
        let want = match &cache.by_url[&slot.0] {
            MediaState::Loading => Showing::Loading,
            MediaState::Image(_) => Showing::Image,
            MediaState::Gif(_) => Showing::Gif,
            MediaState::Refused(reason) => Showing::Refused(reason.clone()),
            MediaState::Failed(reason) => Showing::Failed(reason.clone()),
        };
        if showing == Some(&want) {
            continue;
        }
        let state = cache.by_url[&slot.0].clone();
        let mut e = commands.entity(entity);
        e.despawn_related::<Children>()
            .remove::<(ImageNode, GifAnim)>()
            .insert(want);
        match state {
            MediaState::Loading => {
                e.with_children(|p| {
                    p.spawn(text(&fonts, "Loading image…", Type::META));
                });
            }
            MediaState::Image(handle) => {
                let size = images.get(&handle).map(Image::size);
                e.insert(ImageNode::new(handle));
                size_slot(&mut e, size);
            }
            MediaState::Gif(gif) => {
                e.insert((ImageNode::new(gif.frames[0].clone()), GifAnim::new(&gif)));
                size_slot(&mut e, Some(gif.size));
            }
            MediaState::Refused(reason) | MediaState::Failed(reason) => {
                e.with_children(|p| link_chip(p, &fonts, &slot.0, &reason, false));
            }
        }
    }
}

/// A slot left to size itself takes its picture's size, shrinking to the row but keeping the
/// shape; one with an explicit size (a thumbnail) keeps it.
fn size_slot(e: &mut EntityCommands, size: Option<UVec2>) {
    let Some(size) = size.filter(|s| s.x > 0 && s.y > 0) else {
        return;
    };
    e.entry::<Node>().and_modify(move |mut node| {
        if node.width == Val::Auto && node.height == Val::Auto {
            node.width = px(size.x as f32);
            node.aspect_ratio = Some(size.x as f32 / size.y as f32);
        }
    });
}

/// Marks each GIF as visible when it is shown, inside the window and inside its scroll
/// areas' clip. Pictures that are hidden (another section, another tab) have no size.
fn track_visibility(
    window: Query<&Window, With<PrimaryWindow>>,
    mut gifs: Query<(
        &mut GifAnim,
        &ComputedNode,
        &UiGlobalTransform,
        Option<&CalculatedClip>,
        &InheritedVisibility,
    )>,
) {
    let window = window
        .single()
        .ok()
        .map(|w| Vec2::new(w.physical_width() as f32, w.physical_height() as f32));
    for (mut gif, node, transform, clip, inherited) in &mut gifs {
        let visible = on_screen(
            inherited.get(),
            transform.affine().translation,
            node.size,
            clip.map(|c| c.clip),
            window,
        );
        if gif.visible != visible {
            gif.visible = visible;
        }
    }
}

fn advance_gifs(time: Res<Time>, mut gifs: Query<(&mut GifAnim, &mut ImageNode)>) {
    for (mut gif, mut node) in &mut gifs {
        if gif.visible && gif.advance(time.delta()) {
            node.image = gif.frames[gif.i].clone();
        }
    }
}

/// Whether the window runs at the GIF pace, and how often that was switched.
#[derive(Resource, Debug, Default, Clone, PartialEq, Eq)]
pub struct GifClock {
    /// The wait between wake-ups while a GIF moves; `None` in the normal desktop mode.
    pub waiting: Option<Duration>,
    pub switches: u32,
}

/// While at least one animated GIF is visible the event loop wakes at half its shortest frame
/// delay; when none is left it goes back to the desktop settings. The settings change only at
/// those edges, never per frame (a change makes Bevy redraw at once). Screenshot runs keep
/// their continuous mode.
fn gif_clock(
    mut clock: ResMut<GifClock>,
    gifs: Query<&GifAnim>,
    settings: Option<ResMut<WinitSettings>>,
) {
    let Some(mut settings) = settings else { return };
    let want = gifs
        .iter()
        .filter(|g| g.visible && g.moves())
        .map(GifAnim::shortest)
        .min()
        .map(|d| d / 2);
    match (clock.waiting, want) {
        (None, Some(wait)) if !matches!(settings.focused_mode, UpdateMode::Continuous) => {
            set_pace(&mut settings, wait);
            clock.waiting = Some(wait);
            clock.switches += 1;
        }
        (Some(now), Some(wait)) if wait < now => {
            set_pace(&mut settings, wait);
            clock.waiting = Some(wait);
            clock.switches += 1;
        }
        (Some(_), None) => {
            *settings = WinitSettings::desktop_app();
            clock.waiting = None;
            clock.switches += 1;
        }
        _ => {}
    }
}

fn set_pace(settings: &mut WinitSettings, wait: Duration) {
    settings.focused_mode = UpdateMode::reactive(wait);
    settings.unfocused_mode = UpdateMode::reactive(wait);
}

pub struct MediaPlugin;

impl Plugin for MediaPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<MediaCache>()
            .init_resource::<GifClock>()
            .add_systems(Update, (decode_arrived, fill_slots, advance_gifs).chain())
            // Layout is written in `PostUpdate`; reading it there starts a GIF in the frame
            // it gets a size, before the event loop settles on how long to wait.
            .add_systems(
                PostUpdate,
                (track_visibility, gif_clock)
                    .chain()
                    .after(UiSystems::PostLayout),
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::ecs::system::RunSystemOnce;
    use bevy::time::TimeUpdateStrategy;
    use clusia_core::media::MediaKind;
    use image::codecs::gif::GifEncoder;
    use image::{Delay, Frame, Rgba, RgbaImage};

    use crate::bridge::Tell;
    use crate::platform_open::OpenUrls;
    use crate::snapshot::Snapshot;
    use crate::testing;
    use crate::ui::markdown::parse::{Block, Inline};
    use crate::ui::markdown::{MdLink, RenderOpts, markdown};

    const URL: &str = "https://github.com/user-attachments/assets/1.png";

    fn app() -> App {
        testing::app(Snapshot::default())
    }

    /// An empty `MdImage` slot, as the markdown builder leaves it.
    fn slot(app: &mut App, url: &str) -> Entity {
        let e = app
            .world_mut()
            .spawn((Node::default(), MdImage(url.to_string())))
            .id();
        app.update();
        e
    }

    fn file(path: &std::path::Path, kind: MediaKind) -> MediaFile {
        MediaFile {
            path: path.to_string_lossy().into_owned(),
            kind,
            bytes: std::fs::metadata(path).map_or(0, |m| m.len()),
        }
    }

    fn deliver(app: &mut App, url: &str, answer: Result<MediaFile, MediaError>) {
        testing::tell(
            app,
            Tell::Media {
                url: url.to_string(),
                file: answer,
            },
        );
        testing::settle(app);
    }

    fn write_png(dir: &std::path::Path, name: &str, w: u32, h: u32) -> std::path::PathBuf {
        let path = dir.join(name);
        RgbaImage::from_pixel(w, h, Rgba([30, 160, 60, 255]))
            .save_with_format(&path, ImageFormat::Png)
            .unwrap();
        path
    }

    /// A GIF of `n` solid frames that each last `ms` milliseconds.
    fn write_gif(dir: &std::path::Path, name: &str, n: u8, ms: u32) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut encoder = GifEncoder::new(std::fs::File::create(&path).unwrap());
        for i in 0..n {
            let pixels = RgbaImage::from_pixel(6, 4, Rgba([i * 80, 40, 90, 255]));
            let delay = Delay::from_numer_denom_ms(ms, 1);
            encoder
                .encode_frame(Frame::from_parts(pixels, 0, 0, delay))
                .unwrap();
        }
        drop(encoder);
        path
    }

    fn texts(app: &mut App) -> Vec<String> {
        let mut q = app.world_mut().query::<&Text>();
        q.iter(app.world()).map(|t| t.0.clone()).collect()
    }

    /// Makes the slot look laid out and shown, as the UI plugins would.
    fn show(app: &mut App, e: Entity) {
        app.world_mut().entity_mut(e).insert((
            ComputedNode {
                size: Vec2::new(48.0, 32.0),
                ..default()
            },
            UiGlobalTransform::from_translation(Vec2::new(100.0, 100.0)),
            InheritedVisibility::VISIBLE,
        ));
    }

    fn frame_index(app: &App, e: Entity) -> usize {
        app.world().get::<GifAnim>(e).unwrap().i
    }

    fn stepping(app: &mut App, ms: u64) {
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
            ms,
        )));
    }

    #[test]
    fn a_slot_asks_once_per_url_and_shows_loading() {
        let mut app = app();
        let first = slot(&mut app, URL);
        slot(&mut app, URL);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::FetchMedia(URL.to_string())],
            "two slots, one ask"
        );
        assert_eq!(app.world().get::<Showing>(first), Some(&Showing::Loading));
        assert!(texts(&mut app).contains(&"Loading image…".to_string()));
    }

    #[test]
    fn refused_media_becomes_a_link_chip() {
        let mut app = app();
        let e = slot(&mut app, URL);
        testing::recorded(&mut app);
        deliver(
            &mut app,
            URL,
            Err(MediaError::refused("Images from this site are not allowed")),
        );
        assert!(
            app.world().get::<ImageNode>(e).is_none(),
            "never blank, never a picture"
        );
        let chip = texts(&mut app)
            .into_iter()
            .find(|t| t.ends_with("— open in browser"))
            .expect("a chip");
        assert_eq!(
            chip,
            "Images from this site are not allowed — open in browser"
        );
        let mut q = app.world_mut().query::<(Entity, &MdLink)>();
        let (link, _) = q.single(app.world()).unwrap();
        testing::activate(&mut app, link);
        assert_eq!(app.world().resource::<OpenUrls>().0, [URL]);
        assert!(texts(&mut app).iter().all(|t| t != "Loading image…"));
    }

    #[test]
    fn a_failed_fetch_is_a_chip_too() {
        let mut app = app();
        slot(&mut app, URL);
        deliver(
            &mut app,
            URL,
            Err(MediaError::transient("Not connected to clusiad")),
        );
        assert!(
            texts(&mut app).contains(&"Not connected to clusiad — open in browser".to_string())
        );
    }

    #[test]
    fn a_png_fills_the_slot_and_keeps_its_shape() {
        let dir = tempfile::tempdir().unwrap();
        let png = write_png(dir.path(), "a.png", 8, 4);
        let mut app = app();
        let e = slot(&mut app, URL);
        deliver(&mut app, URL, Ok(file(&png, MediaKind::Png)));
        assert!(app.world().get::<ImageNode>(e).is_some());
        let node = app.world().get::<Node>(e).unwrap();
        assert_eq!((node.width, node.aspect_ratio), (px(8), Some(2.0)));
        assert_eq!(
            node.max_width,
            Val::Auto,
            "the builder's max width stays what it was"
        );
        assert!(matches!(
            app.world().resource::<MediaCache>().by_url[URL],
            MediaState::Image(_)
        ));

        let thumb = app
            .world_mut()
            .spawn((
                Node {
                    width: px(48),
                    height: px(48),
                    ..default()
                },
                MdImage(URL.to_string()),
            ))
            .id();
        testing::settle(&mut app);
        let node = app.world().get::<Node>(thumb).unwrap();
        assert_eq!(
            (node.width, node.height),
            (px(48), px(48)),
            "a thumbnail keeps its size"
        );
        assert!(app.world().get::<ImageNode>(thumb).is_some());
    }

    #[test]
    fn a_jpeg_is_decoded_like_a_png() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jpg");
        DynamicImage::ImageRgb8(image::RgbImage::from_pixel(6, 6, image::Rgb([200, 40, 40])))
            .save_with_format(&path, ImageFormat::Jpeg)
            .unwrap();
        let mut app = app();
        let e = slot(&mut app, URL);
        deliver(&mut app, URL, Ok(file(&path, MediaKind::Jpeg)));
        assert!(app.world().get::<ImageNode>(e).is_some());
        assert_eq!(app.world().get::<Node>(e).unwrap().aspect_ratio, Some(1.0));
    }

    #[test]
    fn pictures_that_are_too_big_or_damaged_are_chips() {
        let dir = tempfile::tempdir().unwrap();
        let wide = write_png(dir.path(), "wide.png", MAX_SIDE + 1, 1);
        let junk = dir.path().join("junk.png");
        std::fs::write(&junk, b"not a picture").unwrap();
        let mut app = app();
        slot(&mut app, "https://github.com/a/wide.png");
        slot(&mut app, "https://github.com/a/junk.png");
        deliver(
            &mut app,
            "https://github.com/a/wide.png",
            Ok(file(&wide, MediaKind::Png)),
        );
        deliver(
            &mut app,
            "https://github.com/a/junk.png",
            Ok(file(&junk, MediaKind::Png)),
        );
        let all = texts(&mut app);
        assert!(all.contains(&"The picture is too large to show — open in browser".to_string()));
        assert!(all.contains(&"The picture is damaged — open in browser".to_string()));
    }

    /// A PNG whose header claims `w` × `h` and which carries no pixel data.
    fn write_png_header(dir: &std::path::Path, w: u32, h: u32) -> std::path::PathBuf {
        fn crc(data: &[u8]) -> u32 {
            let mut c = 0xffff_ffffu32;
            for b in data {
                c ^= u32::from(*b);
                for _ in 0..8 {
                    c = if c & 1 == 1 {
                        (c >> 1) ^ 0xedb8_8320
                    } else {
                        c >> 1
                    };
                }
            }
            !c
        }
        let mut ihdr = b"IHDR".to_vec();
        ihdr.extend(w.to_be_bytes());
        ihdr.extend(h.to_be_bytes());
        ihdr.extend([8, 6, 0, 0, 0]);
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend(13u32.to_be_bytes());
        bytes.extend(&ihdr);
        bytes.extend(crc(&ihdr).to_be_bytes());
        let path = dir.join("huge.png");
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn a_picture_claiming_a_huge_size_is_refused_from_its_header() {
        let dir = tempfile::tempdir().unwrap();
        let huge = write_png_header(dir.path(), 11_000, 11_000);
        let mut app = app();
        slot(&mut app, URL);
        deliver(&mut app, URL, Ok(file(&huge, MediaKind::Png)));
        assert!(
            texts(&mut app)
                .contains(&"The picture is too large to show — open in browser".to_string())
        );
    }

    #[test]
    fn a_failure_while_offline_is_asked_again_after_reconnecting() {
        let mut app = app();
        slot(&mut app, URL);
        testing::recorded(&mut app);
        deliver(
            &mut app,
            URL,
            Err(MediaError::transient("Not connected to clusiad")),
        );
        assert!(testing::recorded(&mut app).is_empty());
        testing::tell(&mut app, Tell::Lost("gone".into()));
        testing::tell(&mut app, Tell::Snapshot(Box::default()));
        testing::settle(&mut app);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::FetchMedia(URL.to_string())],
            "one new ask"
        );
        assert!(texts(&mut app).contains(&"Loading image…".to_string()));
    }

    #[test]
    fn a_failure_coded_offline_or_a_worker_failure_is_retried_once() {
        for message in ["The network is down", "cannot reach clusiad: no socket"] {
            let mut app = app();
            slot(&mut app, URL);
            testing::recorded(&mut app);
            deliver(&mut app, URL, Err(MediaError::transient(message)));
            testing::tell(&mut app, Tell::Lost("gone".into()));
            testing::tell(&mut app, Tell::Snapshot(Box::default()));
            testing::settle(&mut app);
            assert_eq!(
                testing::recorded(&mut app),
                [Ask::FetchMedia(URL.to_string())],
                "{message}"
            );
        }
    }

    #[test]
    fn a_refusal_with_reconnect_words_in_it_stays_cached() {
        let mut app = app();
        slot(&mut app, URL);
        testing::recorded(&mut app);
        deliver(
            &mut app,
            URL,
            Err(MediaError::refused("lost connected timeout")),
        );
        testing::tell(&mut app, Tell::Lost("gone".into()));
        testing::tell(&mut app, Tell::Snapshot(Box::default()));
        testing::settle(&mut app);
        assert!(testing::recorded(&mut app).is_empty());
    }

    #[test]
    fn a_vanished_file_is_fetched_again_once() {
        let mut app = app();
        slot(&mut app, URL);
        testing::recorded(&mut app);
        let gone = file(
            std::path::Path::new("/nonexistent/clusia/a.png"),
            MediaKind::Png,
        );
        deliver(&mut app, URL, Ok(gone.clone()));
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::FetchMedia(URL.to_string())]
        );
        deliver(&mut app, URL, Ok(gone));
        assert!(testing::recorded(&mut app).is_empty(), "only once");
        assert!(
            texts(&mut app).contains(&"The picture cannot be read — open in browser".to_string())
        );
    }

    #[test]
    fn external_images_respect_the_setting() {
        let mut app = app();
        let other = "https://example.com/x.png";
        let blocks = vec![Block::Paragraph(vec![Inline::Image {
            url: other.into(),
            alt: "x".into(),
            space_after: false,
        }])];
        let fonts = UiFonts::default();
        let build = |app: &mut App, load_external: bool| {
            let blocks = blocks.clone();
            let fonts = fonts.clone();
            app.world_mut()
                .run_system_once(move |mut commands: Commands| {
                    commands.spawn(Node::default()).with_children(|p| {
                        let opts = RenderOpts {
                            load_external,
                            ..RenderOpts::default()
                        };
                        markdown(p, &fonts, &blocks, &opts);
                    });
                })
                .unwrap();
            testing::settle(app);
        };
        build(&mut app, false);
        assert!(
            testing::recorded(&mut app).is_empty(),
            "off: nothing is fetched"
        );
        assert!(texts(&mut app).contains(&"Image from another site — open in browser".to_string()));
        build(&mut app, true);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::FetchMedia(other.to_string())]
        );
    }

    #[test]
    fn gif_frames_advance_with_time() {
        let dir = tempfile::tempdir().unwrap();
        let gif = write_gif(dir.path(), "a.gif", 3, 100);
        let mut app = app();
        let e = slot(&mut app, URL);
        deliver(&mut app, URL, Ok(file(&gif, MediaKind::Gif)));
        let MediaState::Gif(frames) = app.world().resource::<MediaCache>().by_url[URL].clone()
        else {
            panic!("a GIF")
        };
        assert_eq!(frames.frames.len(), 3);
        assert_eq!(frames.size, UVec2::new(6, 4));
        assert_eq!(
            app.world().get::<ImageNode>(e).unwrap().image,
            frames.frames[0]
        );
        show(&mut app, e);
        stepping(&mut app, 100);
        let mut seen = HashSet::new();
        for _ in 0..8 {
            app.update();
            seen.insert(frame_index(&app, e));
        }
        assert_eq!(
            seen,
            HashSet::from([0, 1, 2]),
            "it loops through every frame"
        );
        let shown = app.world().get::<ImageNode>(e).unwrap().image.clone();
        assert_eq!(shown, frames.frames[frame_index(&app, e)]);
    }

    #[test]
    fn hidden_gifs_do_not_advance() {
        let dir = tempfile::tempdir().unwrap();
        let gif = write_gif(dir.path(), "a.gif", 3, 100);
        let mut app = app();
        let e = slot(&mut app, URL);
        deliver(&mut app, URL, Ok(file(&gif, MediaKind::Gif)));
        stepping(&mut app, 100);
        let run = |app: &mut App| {
            for _ in 0..6 {
                app.update();
            }
        };
        // Not laid out yet: no size.
        run(&mut app);
        assert_eq!(frame_index(&app, e), 0, "an empty node is not visible");
        show(&mut app, e);
        app.world_mut()
            .entity_mut(e)
            .insert(InheritedVisibility::HIDDEN);
        run(&mut app);
        assert_eq!(frame_index(&app, e), 0, "a hidden node");
        app.world_mut().entity_mut(e).insert((
            InheritedVisibility::VISIBLE,
            CalculatedClip {
                clip: Rect::new(500.0, 500.0, 900.0, 900.0),
            },
        ));
        run(&mut app);
        assert_eq!(frame_index(&app, e), 0, "scrolled out of its clip");
        app.world_mut().entity_mut(e).remove::<CalculatedClip>();
        let mut seen = HashSet::new();
        for _ in 0..6 {
            app.update();
            seen.insert(frame_index(&app, e));
        }
        assert!(app.world().get::<GifAnim>(e).unwrap().visible);
        assert_eq!(seen, HashSet::from([0, 1, 2]), "on screen it moves again");
    }

    #[test]
    fn gif_clock_switches_once() {
        let dir = tempfile::tempdir().unwrap();
        let gif = write_gif(dir.path(), "a.gif", 2, 100);
        let mut app = app();
        app.insert_resource(WinitSettings::desktop_app());
        let a = slot(&mut app, URL);
        let other = "https://github.com/user-attachments/assets/2.png";
        let b = slot(&mut app, other);
        deliver(&mut app, URL, Ok(file(&gif, MediaKind::Gif)));
        deliver(&mut app, other, Ok(file(&gif, MediaKind::Gif)));
        stepping(&mut app, 16);
        for e in [a, b] {
            show(&mut app, e);
        }
        for _ in 0..30 {
            app.update();
        }
        let clock = app.world().resource::<GifClock>().clone();
        assert_eq!(clock.switches, 1, "once, not once per frame");
        let half = Duration::from_millis(50);
        assert_eq!(clock.waiting, Some(half));
        let settings = app.world().resource::<WinitSettings>();
        assert_eq!(settings.focused_mode, UpdateMode::reactive(half));
        assert_eq!(settings.unfocused_mode, UpdateMode::reactive(half));
        for e in [a, b] {
            app.world_mut()
                .entity_mut(e)
                .insert(InheritedVisibility::HIDDEN);
        }
        for _ in 0..5 {
            app.update();
        }
        let clock = app.world().resource::<GifClock>().clone();
        assert_eq!((clock.switches, clock.waiting), (2, None));
        let settings = app.world().resource::<WinitSettings>();
        assert_eq!(
            settings.focused_mode,
            WinitSettings::desktop_app().focused_mode
        );
        assert_eq!(
            settings.unfocused_mode,
            WinitSettings::desktop_app().unfocused_mode
        );
    }

    #[test]
    fn the_clock_switches_in_the_frame_the_gif_gets_a_size() {
        fn lay_out(mut nodes: Query<&mut ComputedNode, With<GifAnim>>) {
            for mut node in &mut nodes {
                node.size = Vec2::new(48.0, 32.0);
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let gif = write_gif(dir.path(), "a.gif", 2, 100);
        let mut app = app();
        app.insert_resource(WinitSettings::desktop_app());
        app.add_systems(PostUpdate, lay_out.in_set(UiSystems::PostLayout));
        let e = slot(&mut app, URL);
        deliver(&mut app, URL, Ok(file(&gif, MediaKind::Gif)));
        app.world_mut().entity_mut(e).insert((
            UiGlobalTransform::from_translation(Vec2::new(100.0, 100.0)),
            InheritedVisibility::VISIBLE,
        ));
        app.update();
        assert_eq!(app.world().resource::<GifClock>().switches, 1);
    }

    #[test]
    fn screenshots_keep_their_continuous_mode() {
        let dir = tempfile::tempdir().unwrap();
        let gif = write_gif(dir.path(), "a.gif", 2, 100);
        let mut app = app();
        app.insert_resource(WinitSettings::continuous());
        let e = slot(&mut app, URL);
        deliver(&mut app, URL, Ok(file(&gif, MediaKind::Gif)));
        show(&mut app, e);
        for _ in 0..5 {
            app.update();
        }
        assert_eq!(app.world().resource::<GifClock>().switches, 0);
        assert_eq!(
            app.world().resource::<WinitSettings>().focused_mode,
            UpdateMode::Continuous
        );
    }

    #[test]
    fn a_still_gif_does_not_wake_the_window() {
        let dir = tempfile::tempdir().unwrap();
        let gif = write_gif(dir.path(), "one.gif", 1, 100);
        let mut app = app();
        app.insert_resource(WinitSettings::desktop_app());
        let e = slot(&mut app, URL);
        deliver(&mut app, URL, Ok(file(&gif, MediaKind::Gif)));
        show(&mut app, e);
        for _ in 0..5 {
            app.update();
        }
        assert_eq!(app.world().resource::<GifClock>().switches, 0);
    }

    #[test]
    fn frames_advance_and_wrap() {
        let mut gif = GifAnim {
            frames: vec![Handle::default(); 3],
            delays: vec![Duration::from_millis(100); 3],
            i: 0,
            t: Duration::ZERO,
            visible: true,
        };
        assert!(!gif.advance(Duration::from_millis(60)));
        assert!(gif.advance(Duration::from_millis(60)));
        assert_eq!((gif.i, gif.t), (1, Duration::from_millis(20)));
        gif.advance(Duration::from_secs(3600));
        assert!(
            gif.i < 3 && gif.t < Duration::from_millis(100),
            "a long pause cannot spin"
        );
        assert_eq!(frame_delay(Duration::ZERO), Duration::from_millis(100));
        assert_eq!(
            frame_delay(Duration::from_millis(40)),
            Duration::from_millis(40)
        );
    }

    #[test]
    fn on_screen_needs_size_visibility_window_and_clip() {
        let centre = Vec2::new(100.0, 100.0);
        let size = Vec2::new(40.0, 40.0);
        let window = Some(Vec2::new(800.0, 600.0));
        assert!(on_screen(true, centre, size, None, window));
        assert!(!on_screen(false, centre, size, None, window));
        assert!(!on_screen(true, centre, Vec2::ZERO, None, window));
        assert!(
            !on_screen(true, Vec2::new(-100.0, 100.0), size, None, window),
            "left of the window"
        );
        assert!(
            !on_screen(true, Vec2::new(100.0, 900.0), size, None, window),
            "below the window"
        );
        let clip = Some(Rect::new(0.0, 300.0, 800.0, 600.0));
        assert!(
            !on_screen(true, centre, size, clip, window),
            "scrolled out of its area"
        );
        assert!(
            on_screen(true, centre, size, None, None),
            "no window: only the node counts"
        );
    }

    #[test]
    fn media_tells_reach_the_cache() {
        let mut app = app();
        deliver(
            &mut app,
            URL,
            Err(MediaError::transient("Not connected to clusiad")),
        );
        assert_eq!(
            app.world().resource::<MediaCache>().by_url[URL],
            MediaState::Failed("Not connected to clusiad".into())
        );
    }

    #[test]
    fn a_refused_url_is_not_asked_again_after_reconnecting() {
        let mut app = app();
        slot(&mut app, URL);
        testing::recorded(&mut app);
        deliver(
            &mut app,
            URL,
            Err(MediaError::refused("Images from this site are not allowed")),
        );
        assert!(testing::recorded(&mut app).is_empty());
        testing::tell(&mut app, Tell::Lost("gone".into()));
        testing::tell(&mut app, Tell::Snapshot(Box::default()));
        testing::settle(&mut app);
        assert!(
            testing::recorded(&mut app).is_empty(),
            "refused URLs are not asked again"
        );
        assert!(
            texts(&mut app)
                .contains(&"Images from this site are not allowed — open in browser".to_string())
        );
    }
}
