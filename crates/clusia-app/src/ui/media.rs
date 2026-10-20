//! Pictures in comments: the cache of fetched images and GIFs, the systems that fill the
//! `MdImage` slots, and the clock that keeps the window awake only while a GIF is moving.
//!
//! The window reads only files whose path the daemon returned; a missing file is fetched again
//! once, then reported.

use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;

use bevy::asset::RenderAssetUsages;
use bevy::prelude::*;
use bevy::tasks::{AsyncComputeTaskPool, Task};
use bevy::ui::{CalculatedClip, ComputedNode, UiGlobalTransform, UiSystems};
use bevy::window::PrimaryWindow;
use bevy::winit::{UpdateMode, WinitSettings};
use clusia_core::PrRef;
use clusia_core::media::MediaKind;
use clusia_protocol::MediaFile;
use image::{
    AnimationDecoder, DynamicImage, ImageDecoder, ImageError, ImageFormat, ImageReader, Limits,
};

use crate::bridge::{Ask, Asks, MediaError};
use crate::fonts::UiFonts;
use crate::nav::ReviewScreen;
use crate::review_state::ReviewTabs;
use crate::ui::composer::popover::sync_popovers;
use crate::ui::kit::{Type, wrapped_text};
use crate::ui::lru::ByteLru;
use crate::ui::markdown::{MdImage, link_chip};
use crate::ui::pace::WindowCovered;

/// Frames kept from one GIF.
pub const MAX_GIF_FRAMES: usize = 300;
/// The longest side, in pixels, of a picture the window decodes.
pub const MAX_SIDE: u32 = 4096;
/// Frames stop being kept once a GIF holds this many pixels in total.
const MAX_GIF_PIXELS: u64 = 48_000_000;
/// Browsers show GIF frames that ask for less than this for this long instead.
const MIN_FRAME: Duration = Duration::from_millis(20);
const DEFAULT_FRAME: Duration = Duration::from_millis(100);
/// The shortest wait between wakes for GIFs (about 30 a second). `GifAnim::advance` carries
/// the time left over, so a faster GIF skips frames instead of running slow.
const MIN_WAKE_WAIT: Duration = Duration::from_millis(33);

/// The frames of an animated GIF, each a full picture.
#[derive(Debug, Clone, PartialEq)]
pub struct GifFrames {
    /// The first frame as a texture, for places that show a GIF as a still.
    pub first: Handle<Image>,
    /// The RGBA pixels of every frame, shared by the slots that show the GIF.
    pub pixels: Arc<Vec<Vec<u8>>>,
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

/// The decoded pixels the cache keeps, at most: a quarter of a gigabyte. One GIF at the frame
/// cap is about 192 MB, so it fits with room for the stills around it.
pub const MEDIA_CACHE_BYTES: usize = 256 << 20;

/// What the daemon fetched for each URL, decoded. One entry per URL, so a picture used twice is
/// asked for once. Past `MEDIA_CACHE_BYTES` the least recently used pictures are forgotten,
/// except the ones on screen: a still whose texture a node holds, a GIF that is visible.
#[derive(Resource, Debug)]
pub struct MediaCache {
    pub by_url: ByteLru<String, MediaState>,
    /// Answers waiting to be decoded.
    arrived: Vec<(String, Result<MediaFile, MediaError>)>,
    /// Files being read and decoded off the frame.
    decoding: Vec<(String, Task<Read>)>,
    /// Decodes that finished, waiting to be cached.
    done: Vec<(String, Read)>,
    /// URLs whose cached file was gone and have been asked for again.
    retried: HashSet<String>,
    /// The review tabs whose screens showed each URL. A URL left with none (every one closed)
    /// is forgotten as soon as nothing shows it.
    owners: HashMap<String, HashSet<PrRef>>,
}

impl Default for MediaCache {
    fn default() -> Self {
        Self::with_cap(MEDIA_CACHE_BYTES)
    }
}

impl MediaCache {
    pub fn with_cap(cap: usize) -> Self {
        Self {
            by_url: ByteLru::new(cap),
            arrived: Vec::new(),
            decoding: Vec::new(),
            done: Vec::new(),
            retried: HashSet::new(),
            owners: HashMap::new(),
        }
    }

    /// Queues the daemon's answer for `url`.
    pub fn arrive(&mut self, url: String, file: Result<MediaFile, MediaError>) {
        self.arrived.push((url, file));
    }

    /// What the cache holds for `url`, without counting it as used.
    pub fn state(&self, url: &str) -> Option<&MediaState> {
        self.by_url.peek(&url.to_string())
    }

    /// What the cache holds for `url`, marked as used; the first want asks the daemon for it.
    pub fn want(&mut self, url: &str, asks: &mut Asks) -> &MediaState {
        let key = url.to_string();
        if !self.by_url.contains_key(&key) {
            self.by_url.insert(key.clone(), MediaState::Loading, 0);
            asks.send(Ask::FetchMedia(key.clone()));
        }
        self.by_url.get(&key).expect("inserted above")
    }

    /// Forgets the pictures that failed for a reason that may have passed (the connection, the
    /// daemon's network), so their slots ask again. Refused pictures stay cached: asking again
    /// would get the same answer.
    pub fn retry_failed(&mut self) {
        self.by_url
            .retain(|_, state| !matches!(state, MediaState::Failed(_)));
        self.retried.clear();
    }

    /// Forgets what is over the cap and what only closed tabs showed, keeping what is on
    /// screen. Returns the URLs of the GIFs forgotten, whose hidden slots must let go of
    /// their frames.
    fn trim(&mut self, visible_gifs: &HashSet<&str>) -> Vec<String> {
        let in_use = |url: &String, state: &MediaState| match state {
            MediaState::Image(handle) => held_elsewhere(handle),
            MediaState::Gif(gif) => {
                held_elsewhere(&gif.first) || visible_gifs.contains(url.as_str())
            }
            MediaState::Loading | MediaState::Refused(_) | MediaState::Failed(_) => true,
        };
        let mut gone: Vec<(String, MediaState)> = self.by_url.evict(&in_use);
        let orphans: Vec<String> = self
            .owners
            .iter()
            .filter(|(_, tabs)| tabs.is_empty())
            .map(|(url, _)| url.clone())
            .collect();
        for url in orphans {
            match self.by_url.peek(&url) {
                Some(state) if in_use(&url, state) => continue,
                Some(_) => gone.extend(self.by_url.remove(&url).map(|s| (url.clone(), s))),
                None => {}
            }
            self.owners.remove(&url);
        }
        gone.into_iter()
            .filter_map(|(url, state)| {
                self.owners.remove(&url);
                matches!(state, MediaState::Gif(_)).then_some(url)
            })
            .collect()
    }
}

/// Whether something besides the cache holds the texture: a node or an avatar showing it.
fn held_elsewhere(handle: &Handle<Image>) -> bool {
    matches!(handle, Handle::Strong(strong) if Arc::strong_count(strong) > 1)
}

/// Blocks on the decodes in flight, so a test sees their pictures at the next frame.
#[cfg(test)]
pub(crate) fn finish_decodes(world: &mut World) {
    let mut cache = world.resource_mut::<MediaCache>();
    for (url, task) in std::mem::take(&mut cache.decoding) {
        let read = bevy::tasks::block_on(task);
        cache.done.push((url, read));
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

/// A GIF slot whose frames the cache forgot while it was off screen. It keeps showing its last
/// frame; `visible`, kept up to date by `track_visibility`, makes it ask for the GIF again.
#[derive(Component, Debug, Default)]
struct Dormant {
    visible: bool,
}

/// An animated picture. `visible` is kept up to date by `track_visibility`. The slot's
/// `ImageNode` keeps pointing at `shown`; a new frame is written into that texture, because
/// swapping the handle makes Bevy rewrite the node's `ContentSize` and lay the page out again.
#[derive(Component, Debug, Clone, PartialEq)]
pub struct GifAnim {
    pub pixels: Arc<Vec<Vec<u8>>>,
    pub shown: Handle<Image>,
    pub delays: Vec<Duration>,
    pub i: usize,
    pub t: Duration,
    pub visible: bool,
}

impl GifAnim {
    pub fn new(gif: &GifFrames, shown: Handle<Image>) -> Self {
        Self {
            pixels: gif.pixels.clone(),
            shown,
            delays: gif.delays.clone(),
            i: 0,
            t: Duration::ZERO,
            visible: false,
        }
    }

    /// Whether it has more than one frame to show.
    pub fn moves(&self) -> bool {
        self.delays.len() > 1
    }

    /// Moves `dt` forward; `true` when the frame changed.
    pub fn advance(&mut self, dt: Duration) -> bool {
        if !self.moves() {
            return false;
        }
        self.t += dt;
        let start = self.i;
        // A long pause (a closed laptop) must not spin through the loop for ages.
        for _ in 0..self.delays.len() {
            let delay = self.delays[self.i];
            if self.t < delay {
                return self.i != start;
            }
            self.t -= delay;
            self.i = (self.i + 1) % self.delays.len();
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

/// A picture decoded off the frame, still to be added to `Assets<Image>`.
#[derive(Debug)]
enum Decoded {
    Still(Image),
    Gif {
        first: Image,
        pixels: Vec<Vec<u8>>,
        delays: Vec<Duration>,
        size: UVec2,
    },
    Refused(String),
}

/// What a decode task hands back: the picture, or why its file could not be read.
type Read = std::io::Result<Decoded>;

/// Reads and decodes the file; it runs on `AsyncComputeTaskPool`, never inside a frame, since a
/// GIF can take hundreds of milliseconds.
fn read_and_decode(file: &MediaFile) -> Read {
    let bytes = std::fs::read(&file.path)?;
    let decoded = match file.kind {
        MediaKind::Gif => decode_gif(&bytes),
        MediaKind::Png => decode_still(&bytes, ImageFormat::Png),
        MediaKind::Jpeg => decode_still(&bytes, ImageFormat::Jpeg),
    };
    Ok(decoded.unwrap_or_else(Decoded::Refused))
}

fn decode_still(bytes: &[u8], format: ImageFormat) -> Result<Decoded, String> {
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
    Ok(Decoded::Still(Image::from_dynamic(
        picture,
        true,
        RenderAssetUsages::RENDER_WORLD,
    )))
}

fn decode_gif(bytes: &[u8]) -> Result<Decoded, String> {
    let damaged = || "The GIF is damaged".to_string();
    let decoder = image::codecs::gif::GifDecoder::new(Cursor::new(bytes)).map_err(|_| damaged())?;
    let (width, height) = decoder.dimensions();
    if width > MAX_SIDE || height > MAX_SIDE {
        return Err("The GIF is too large to show".into());
    }
    let mut first = None;
    let mut pixels = Vec::new();
    let mut delays = Vec::new();
    for frame in decoder.into_frames().take(MAX_GIF_FRAMES) {
        let Ok(frame) = frame else { break };
        // Keeps at least one frame, then stops before the total passes the cap.
        let total = (pixels.len() as u64 + 1) * u64::from(width) * u64::from(height);
        if !pixels.is_empty() && total > MAX_GIF_PIXELS {
            break;
        }
        delays.push(frame_delay(Duration::from(frame.delay())));
        let buffer = frame.into_buffer();
        // `shown_texture` builds each picture from `width × height` pixels, so a frame the
        // decoder returned at another size is refused here, not left to panic there.
        if buffer.dimensions() != (width, height) {
            return Err(damaged());
        }
        if first.is_none() {
            first = Some(Image::from_dynamic(
                DynamicImage::ImageRgba8(buffer.clone()),
                true,
                RenderAssetUsages::RENDER_WORLD,
            ));
        }
        pixels.push(buffer.into_raw());
    }
    let Some(first) = first else {
        return Err(damaged());
    };
    Ok(Decoded::Gif {
        first,
        pixels,
        delays,
        size: UVec2::new(width, height),
    })
}

/// The cache entry for a decoded picture and the bytes it holds: the RGBA of a still's texture,
/// or of every GIF frame plus its first-frame texture.
fn cache_entry(decoded: Decoded, images: &mut Assets<Image>) -> (MediaState, usize) {
    let rgba = |size: UVec2| size.x as usize * size.y as usize * 4;
    match decoded {
        Decoded::Still(image) => {
            let bytes = rgba(image.size());
            (MediaState::Image(images.add(image)), bytes)
        }
        Decoded::Gif {
            first,
            pixels,
            delays,
            size,
        } => {
            let bytes = pixels.iter().map(Vec::len).sum::<usize>() + rgba(size);
            let gif = GifFrames {
                first: images.add(first),
                pixels: Arc::new(pixels),
                delays,
                size,
            };
            (MediaState::Gif(gif), bytes)
        }
        Decoded::Refused(reason) => (MediaState::Refused(reason), 0),
    }
}

/// The URLs of the GIFs on screen, which the cache must keep.
fn visible_gifs<'a>(gifs: &'a Query<(&MdImage, &GifAnim)>) -> HashSet<&'a str> {
    gifs.iter()
        .filter(|(_, g)| g.visible)
        .map(|(slot, _)| slot.0.as_str())
        .collect()
}

/// Caches the decodes that finished, starts one for each new answer, and trims the cache. The
/// polling comes before the starting, so a file is never decoded in the frame it arrived in.
fn decode_arrived(
    mut commands: Commands,
    mut cache: ResMut<MediaCache>,
    mut images: ResMut<Assets<Image>>,
    mut asks: ResMut<Asks>,
    gifs: Query<(&MdImage, &GifAnim)>,
    slots: Query<(Entity, &MdImage), With<GifAnim>>,
) {
    let cache = &mut *cache;
    // Trimmed before this frame's pictures land: their slots take their textures in this
    // frame, and from then on the textures count as held.
    let forgotten = cache.trim(&visible_gifs(&gifs));
    if !forgotten.is_empty() {
        for (entity, slot) in &slots {
            if forgotten.contains(&slot.0) {
                commands
                    .entity(entity)
                    .remove::<GifAnim>()
                    .insert(Dormant::default());
            }
        }
    }
    cache.decoding.retain_mut(|(url, task)| {
        match bevy::tasks::block_on(bevy::tasks::poll_once(task)) {
            Some(read) => {
                cache.done.push((url.clone(), read));
                false
            }
            None => true,
        }
    });
    for (url, read) in std::mem::take(&mut cache.done) {
        let (state, bytes) = match read {
            Ok(decoded) => cache_entry(decoded, &mut images),
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    && cache.retried.insert(url.clone()) =>
            {
                asks.send(Ask::FetchMedia(url.clone()));
                (MediaState::Loading, 0)
            }
            Err(_) => (MediaState::Refused("The picture cannot be read".into()), 0),
        };
        cache.by_url.insert(url, state, bytes);
    }
    for (url, answer) in std::mem::take(&mut cache.arrived) {
        let state = match answer {
            Err(e) if e.transient => MediaState::Failed(e.message),
            Err(e) => MediaState::Refused(e.message),
            Ok(file) => {
                let task = AsyncComputeTaskPool::get().spawn(async move { read_and_decode(&file) });
                cache.decoding.push((url.clone(), task));
                if cache.by_url.contains_key(&url) {
                    continue;
                }
                MediaState::Loading
            }
        };
        cache.by_url.insert(url, state, 0);
    }
}

/// Lets go of the closed tabs: the pictures only they showed are forgotten once nothing shows
/// them (`MediaCache::trim`).
fn forget_closed_tabs(tabs: Res<ReviewTabs>, mut cache: ResMut<MediaCache>) {
    for owners in cache.owners.values_mut() {
        owners.retain(|pr| tabs.0.contains_key(pr));
    }
}

fn showing_of(state: &MediaState) -> Showing {
    match state {
        MediaState::Loading => Showing::Loading,
        MediaState::Image(_) => Showing::Image,
        MediaState::Gif(_) => Showing::Gif,
        MediaState::Refused(reason) => Showing::Refused(reason.clone()),
        MediaState::Failed(reason) => Showing::Failed(reason.clone()),
    }
}

/// Asks for every picture a slot wants and shows what the cache holds.
fn fill_slots(
    mut commands: Commands,
    fonts: Res<UiFonts>,
    mut images: ResMut<Assets<Image>>,
    mut cache: ResMut<MediaCache>,
    mut asks: ResMut<Asks>,
    slots: Query<(Entity, &MdImage, Option<&Showing>, Option<&Dormant>)>,
    parents: Query<&ChildOf>,
    screens: Query<&ReviewScreen>,
) {
    for (entity, slot, showing, dormant) in &slots {
        // A slot whose GIF was forgotten keeps its last frame until it is on screen again.
        if dormant.is_some_and(|d| !d.visible) {
            continue;
        }
        let want = showing_of(cache.want(&slot.0, &mut asks));
        match dormant {
            Some(_) if want == Showing::Loading => continue,
            Some(_) => {}
            None if showing == Some(&want) => continue,
            None => {}
        }
        if showing.is_none()
            && let Some(ReviewScreen(pr)) = parents
                .iter_ancestors(entity)
                .find_map(|a| screens.get(a).ok())
        {
            cache
                .owners
                .entry(slot.0.clone())
                .or_default()
                .insert(pr.clone());
        }
        let state = cache.state(&slot.0).cloned().expect("wanted above");
        let mut e = commands.entity(entity);
        e.despawn_related::<Children>()
            .remove::<(ImageNode, GifAnim, Dormant)>()
            .insert(want);
        match state {
            MediaState::Loading => {
                e.with_children(|p| {
                    p.spawn(wrapped_text(&fonts, "Loading image…", Type::META));
                });
            }
            MediaState::Image(handle) => {
                let size = images.get(&handle).map(Image::size);
                e.insert(ImageNode::new(handle));
                size_slot(&mut e, size);
            }
            MediaState::Gif(gif) => {
                let shown = images.add(shown_texture(&gif));
                e.insert((ImageNode::new(shown.clone()), GifAnim::new(&gif, shown)));
                size_slot(&mut e, Some(gif.size));
            }
            MediaState::Refused(reason) | MediaState::Failed(reason) => {
                e.with_children(|p| link_chip(p, &fonts, &slot.0, &reason, false));
            }
        }
    }
}

/// The texture one slot shows its GIF in, starting on the first frame. It keeps its pixels in
/// the main world so a frame can be written into it.
fn shown_texture(gif: &GifFrames) -> Image {
    let frame = image::RgbaImage::from_raw(gif.size.x, gif.size.y, gif.pixels[0].clone())
        .expect("every frame holds width × height RGBA pixels");
    Image::from_dynamic(
        DynamicImage::ImageRgba8(frame),
        true,
        RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
    )
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
/// areas' clip. Pictures that are hidden (another section, another tab) have no size, and
/// none counts while the window is covered.
/// Where a slot sits, for `on_screen`.
type Placed<'a> = (
    &'a ComputedNode,
    &'a UiGlobalTransform,
    Option<&'a CalculatedClip>,
    &'a InheritedVisibility,
);

fn track_visibility(
    covered: Res<WindowCovered>,
    window: Query<&Window, With<PrimaryWindow>>,
    mut gifs: Query<(&mut GifAnim, Placed)>,
    mut dormant: Query<(&mut Dormant, Placed), Without<GifAnim>>,
) {
    let window = window
        .single()
        .ok()
        .map(|w| Vec2::new(w.physical_width() as f32, w.physical_height() as f32));
    let seen = |(node, transform, clip, inherited): Placed| {
        !covered.0
            && on_screen(
                inherited.get(),
                transform.affine().translation,
                node.size,
                clip.map(|c| c.clip),
                window,
            )
    };
    for (mut gif, placed) in &mut gifs {
        let visible = seen(placed);
        if gif.visible != visible {
            gif.visible = visible;
        }
    }
    for (mut slot, placed) in &mut dormant {
        let visible = seen(placed);
        if slot.visible != visible {
            slot.visible = visible;
        }
    }
}

fn advance_gifs(time: Res<Time>, mut images: ResMut<Assets<Image>>, mut gifs: Query<&mut GifAnim>) {
    for mut gif in &mut gifs {
        if gif.visible && gif.advance(time.delta()) {
            let gif = &*gif;
            if let Some(mut texture) = images.get_mut(&gif.shown)
                && let Some(data) = texture.data.as_mut()
            {
                data.copy_from_slice(&gif.pixels[gif.i]);
            }
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
/// delay, but never sooner than `MIN_WAKE_WAIT`; when none is left it goes back to the desktop
/// settings. The settings change only at those edges, never per frame (a change makes Bevy
/// redraw at once). Screenshot runs keep their continuous mode.
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
        .map(|d| (d / 2).max(MIN_WAKE_WAIT));
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
            .add_systems(
                Update,
                // Slots inside a popover are despawned with it; filling them afterwards
                // would touch a gone entity.
                (decode_arrived, fill_slots, advance_gifs)
                    .chain()
                    .after(sync_popovers),
            )
            .add_systems(
                Update,
                forget_closed_tabs
                    .before(decode_arrived)
                    .run_if(resource_changed::<ReviewTabs>),
            )
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
    use bevy::window::WindowOccluded;
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
        testing::decoded(app);
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

    fn state(app: &App, url: &str) -> Option<MediaState> {
        app.world().resource::<MediaCache>().state(url).cloned()
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
        assert!(matches!(state(&app, URL), Some(MediaState::Image(_))));

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
        let Some(MediaState::Gif(frames)) = state(&app, URL) else {
            panic!("a GIF")
        };
        assert_eq!(frames.pixels.len(), 3);
        assert_eq!(frames.size, UVec2::new(6, 4));
        let images = app.world().resource::<Assets<Image>>();
        let node = app.world().get::<ImageNode>(e).unwrap();
        assert_eq!(
            red_of(images, &node.image),
            0,
            "it starts on the first frame"
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
        let images = app.world().resource::<Assets<Image>>();
        assert_eq!(red_of(images, &shown), frame_index(&app, e) as u8 * 80);
    }

    /// The first byte of each of the three solid frames `write_gif` makes.
    fn red_of(images: &Assets<Image>, handle: &Handle<Image>) -> u8 {
        images.get(handle).unwrap().data.as_ref().unwrap()[0]
    }

    #[test]
    fn a_gif_frame_swaps_pixels_without_touching_the_node() {
        // Bevy recomputes a node's `ContentSize` (and so the layout) only when its `ImageNode`
        // changed; this app has no UI plugin, so an untouched node is what the test can pin.
        #[derive(Resource, Default)]
        struct Touched(u32);
        fn count(mut touched: ResMut<Touched>, nodes: Query<(), Changed<ImageNode>>) {
            touched.0 += nodes.iter().count() as u32;
        }
        let dir = tempfile::tempdir().unwrap();
        let gif = write_gif(dir.path(), "a.gif", 3, 100);
        let mut app = app();
        app.init_resource::<Touched>().add_systems(Last, count);
        let e = slot(&mut app, URL);
        deliver(&mut app, URL, Ok(file(&gif, MediaKind::Gif)));
        show(&mut app, e);
        stepping(&mut app, 100);
        for _ in 0..3 {
            app.update();
        }
        let shown = app.world().get::<ImageNode>(e).unwrap().image.clone();
        let assets = app.world().resource::<Assets<Image>>().len();
        *app.world_mut().resource_mut::<Touched>() = Touched::default();
        let mut reds = HashSet::new();
        for _ in 0..9 {
            app.update();
            let images = app.world().resource::<Assets<Image>>();
            reds.insert(red_of(images, &shown));
            assert_eq!(
                red_of(images, &shown),
                frame_index(&app, e) as u8 * 80,
                "the shown picture is the current frame"
            );
        }
        assert_eq!(reds.len(), 3, "every frame reached the one picture");
        assert_eq!(app.world().get::<ImageNode>(e).unwrap().image, shown);
        assert_eq!(app.world().resource::<Assets<Image>>().len(), assets);
        assert_eq!(
            app.world().resource::<Touched>().0,
            0,
            "no frame rewrites the node, which is what makes Bevy lay the page out again"
        );
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
    fn an_occluded_window_stops_its_gifs_and_the_clock_until_restored() {
        let dir = tempfile::tempdir().unwrap();
        let gif = write_gif(dir.path(), "a.gif", 3, 100);
        let mut app = app();
        app.insert_resource(WinitSettings::desktop_app());
        let e = slot(&mut app, URL);
        deliver(&mut app, URL, Ok(file(&gif, MediaKind::Gif)));
        show(&mut app, e);
        stepping(&mut app, 16);
        for _ in 0..10 {
            app.update();
        }
        assert!(app.world().resource::<GifClock>().waiting.is_some());
        let occlude = |app: &mut App, occluded: bool| {
            app.world_mut().write_message(WindowOccluded {
                window: Entity::PLACEHOLDER,
                occluded,
            });
        };
        occlude(&mut app, true);
        for _ in 0..3 {
            app.update();
        }
        let frozen = frame_index(&app, e);
        let switches = app.world().resource::<GifClock>().switches;
        for _ in 0..40 {
            app.update();
        }
        assert_eq!(frame_index(&app, e), frozen, "no frame while occluded");
        assert!(!app.world().get::<GifAnim>(e).unwrap().visible);
        let clock = app.world().resource::<GifClock>().clone();
        assert_eq!(clock.waiting, None, "the window goes back to sleeping");
        assert_eq!(clock.switches, switches, "and stays there");
        occlude(&mut app, false);
        let mut seen = HashSet::new();
        for _ in 0..40 {
            app.update();
            seen.insert(frame_index(&app, e));
        }
        assert!(seen.len() > 1, "restored, it moves again");
        assert!(app.world().resource::<GifClock>().waiting.is_some());
    }

    #[test]
    fn occlusion_takes_the_spinners_wake_away_too_and_restoring_brings_it_back() {
        use crate::ui::pace::Pace;
        use crate::ui::spinner::spinner;
        let dir = tempfile::tempdir().unwrap();
        let gif = write_gif(dir.path(), "a.gif", 3, 100);
        let mut app = app();
        app.insert_resource(WinitSettings::desktop_app());
        app.world_mut().spawn(spinner());
        let e = slot(&mut app, URL);
        deliver(&mut app, URL, Ok(file(&gif, MediaKind::Gif)));
        show(&mut app, e);
        stepping(&mut app, 16);
        for _ in 0..10 {
            app.update();
        }
        let spin = Duration::from_millis(200);
        let gif_wait = Duration::from_millis(50);
        let focused = |app: &App| app.world().resource::<WinitSettings>().focused_mode;
        assert_eq!(
            focused(&app),
            UpdateMode::reactive(gif_wait),
            "the GIF wakes sooner than the spinner"
        );
        app.world_mut().write_message(WindowOccluded {
            window: Entity::PLACEHOLDER,
            occluded: true,
        });
        for _ in 0..10 {
            app.update();
        }
        assert_eq!(app.world().resource::<GifClock>().waiting, None);
        let desktop = WinitSettings::desktop_app();
        assert_eq!(focused(&app), desktop.focused_mode, "nothing wakes it");
        assert_eq!(
            app.world().resource::<WinitSettings>().unfocused_mode,
            desktop.unfocused_mode
        );
        assert_eq!(app.world().resource::<Pace>().written, None);
        let switches = app.world().resource::<GifClock>().switches;
        for _ in 0..40 {
            app.update();
        }
        assert_eq!(app.world().resource::<GifClock>().switches, switches);
        assert_eq!(focused(&app), desktop.focused_mode, "and it stays asleep");
        app.world_mut().write_message(WindowOccluded {
            window: Entity::PLACEHOLDER,
            occluded: false,
        });
        for _ in 0..10 {
            app.update();
        }
        assert_eq!(focused(&app), UpdateMode::reactive(gif_wait));
        app.world_mut().despawn(e);
        for _ in 0..5 {
            app.update();
        }
        assert_eq!(focused(&app), UpdateMode::reactive(spin));
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
        let fast = write_gif(dir.path(), "fast.gif", 2, 20);
        let quick = "https://github.com/user-attachments/assets/3.gif";
        let c = slot(&mut app, quick);
        deliver(&mut app, quick, Ok(file(&fast, MediaKind::Gif)));
        show(&mut app, c);
        for _ in 0..5 {
            app.update();
        }
        let waiting = app.world().resource::<GifClock>().waiting;
        assert!(
            waiting.is_some_and(|w| w >= Duration::from_millis(33)),
            "never faster than about 30 wakes a second: {waiting:?}"
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
            pixels: Arc::new(vec![Vec::new(); 3]),
            shown: Handle::default(),
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
            state(&app, URL),
            Some(MediaState::Failed("Not connected to clusiad".into()))
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

    /// A 2 × 2 solid PNG is 16 bytes of pixels, a `write_gif` GIF 6 × 4 × 4 = 96 per frame.
    fn capped(app: &mut App, cap: usize) {
        app.insert_resource(MediaCache::with_cap(cap));
    }

    fn png_url(i: usize) -> String {
        format!("https://github.com/user-attachments/assets/p{i}.png")
    }

    /// Shows the PNG of `url` in a slot, then takes the slot away, leaving the cache holding it.
    fn load_png(app: &mut App, dir: &std::path::Path, url: &str) {
        let name = url.rsplit('/').next().expect("a file name");
        let path = write_png(dir, name, 2, 2);
        let e = slot(app, url);
        deliver(app, url, Ok(file(&path, MediaKind::Png)));
        assert!(app.world().get::<ImageNode>(e).is_some());
        app.world_mut().despawn(e);
        app.update();
    }

    fn cached(app: &App) -> Vec<String> {
        let mut urls: Vec<String> = app
            .world()
            .resource::<MediaCache>()
            .by_url
            .keys()
            .cloned()
            .collect();
        urls.sort();
        urls
    }

    #[test]
    fn a_picture_is_decoded_off_the_frame_while_gifs_keep_moving() {
        let dir = tempfile::tempdir().unwrap();
        let gif = write_gif(dir.path(), "a.gif", 3, 32);
        let big = write_png(dir.path(), "big.png", 2048, 2048);
        let mut app = app();
        let moving = slot(&mut app, URL);
        deliver(&mut app, URL, Ok(file(&gif, MediaKind::Gif)));
        show(&mut app, moving);
        stepping(&mut app, 16);
        app.update();
        let other = png_url(1);
        let e = slot(&mut app, &other);
        let before = frame_index(&app, moving);
        testing::tell(
            &mut app,
            Tell::Media {
                url: other.clone(),
                file: Ok(file(&big, MediaKind::Png)),
            },
        );
        assert_eq!(
            state(&app, &other),
            Some(MediaState::Loading),
            "the frame the file arrived in did not decode it"
        );
        assert_eq!(app.world().resource::<MediaCache>().decoding.len(), 1);
        assert!(texts(&mut app).contains(&"Loading image…".to_string()));
        app.update();
        assert_ne!(
            frame_index(&app, moving),
            before,
            "the GIF ticks on while the picture decodes"
        );
        testing::decoded(&mut app);
        assert!(matches!(state(&app, &other), Some(MediaState::Image(_))));
        assert!(app.world().get::<ImageNode>(e).is_some());
    }

    #[test]
    fn the_cache_stays_under_its_cap_and_forgets_the_least_recently_used() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app();
        capped(&mut app, 40);
        let (a, b, c) = (png_url(1), png_url(2), png_url(3));
        load_png(&mut app, dir.path(), &a);
        load_png(&mut app, dir.path(), &b);
        assert_eq!(cached(&app), [a.clone(), b.clone()]);
        // Showing `a` again uses it: `b` is now the least recently used.
        let again = slot(&mut app, &a);
        assert!(app.world().get::<ImageNode>(again).is_some());
        app.world_mut().despawn(again);
        load_png(&mut app, dir.path(), &c);
        assert_eq!(cached(&app), [a, c]);
        let cache = app.world().resource::<MediaCache>();
        assert_eq!(cache.by_url.bytes(), 32);
        assert!(cache.by_url.bytes() <= cache.by_url.cap());
        assert_eq!(MediaCache::default().by_url.cap(), MEDIA_CACHE_BYTES);
    }

    #[test]
    fn a_shown_picture_is_never_evicted_from_under_its_node() {
        let dir = tempfile::tempdir().unwrap();
        let gif = write_gif(dir.path(), "a.gif", 3, 32);
        let mut app = app();
        capped(&mut app, 1);
        let still = png_url(1);
        let path = write_png(dir.path(), "s.png", 2, 2);
        let s = slot(&mut app, &still);
        deliver(&mut app, &still, Ok(file(&path, MediaKind::Png)));
        let g = slot(&mut app, URL);
        // Laid out on screen before its frames arrive, as a GIF in view is.
        show(&mut app, g);
        deliver(&mut app, URL, Ok(file(&gif, MediaKind::Gif)));
        stepping(&mut app, 16);
        app.update();
        testing::recorded(&mut app);
        load_png(&mut app, dir.path(), &png_url(2));
        load_png(&mut app, dir.path(), &png_url(3));
        assert_eq!(
            cached(&app),
            [URL.to_string(), still.clone()],
            "both still shown"
        );
        let shown = app.world().get::<ImageNode>(s).unwrap().image.clone();
        let images = app.world().resource::<Assets<Image>>();
        assert!(images.get(&shown).is_some());
        let mut seen = HashSet::new();
        for _ in 0..8 {
            app.update();
            seen.insert(frame_index(&app, g));
        }
        assert_eq!(seen.len(), 3, "the GIF still moves");
        let asked = testing::recorded(&mut app);
        assert!(
            !asked
                .iter()
                .any(|a| *a == Ask::FetchMedia(URL.into()) || *a == Ask::FetchMedia(still.clone())),
            "neither is asked for again: {asked:?}"
        );
    }

    #[test]
    fn an_evicted_gif_frees_its_frames_and_decodes_again_when_shown() {
        let dir = tempfile::tempdir().unwrap();
        let gif = write_gif(dir.path(), "a.gif", 3, 32);
        let mut app = app();
        capped(&mut app, 400);
        let g = slot(&mut app, URL);
        deliver(&mut app, URL, Ok(file(&gif, MediaKind::Gif)));
        let frames = Arc::downgrade(&app.world().get::<GifAnim>(g).unwrap().pixels);
        // Scrolled away: the slot stays, the GIF is not shown.
        app.update();
        assert!(!app.world().get::<GifAnim>(g).unwrap().visible);
        let big = write_png(dir.path(), "big.png", 10, 10);
        let other = png_url(1);
        let e = slot(&mut app, &other);
        deliver(&mut app, &other, Ok(file(&big, MediaKind::Png)));
        assert!(app.world().get::<ImageNode>(e).is_some());
        assert_eq!(cached(&app), [other], "the GIF was the least recently used");
        assert!(
            frames.upgrade().is_none(),
            "nothing holds the GIF's frames any more"
        );
        assert!(app.world().get::<GifAnim>(g).is_none());
        assert!(
            app.world().get::<ImageNode>(g).is_some(),
            "the slot keeps its last frame, so the page does not move"
        );
        testing::recorded(&mut app);
        for _ in 0..5 {
            app.update();
        }
        assert!(
            testing::recorded(&mut app).is_empty(),
            "a GIF that is not shown is not asked for again"
        );
        app.world_mut().despawn(e);
        show(&mut app, g);
        testing::settle(&mut app);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::FetchMedia(URL.to_string())]
        );
        deliver(&mut app, URL, Ok(file(&gif, MediaKind::Gif)));
        stepping(&mut app, 16);
        let mut seen = HashSet::new();
        for _ in 0..8 {
            app.update();
            seen.insert(frame_index(&app, g));
        }
        assert_eq!(seen, HashSet::from([0, 1, 2]), "decoded again, it animates");
    }

    #[test]
    fn closing_a_tab_drops_its_pictures() {
        use crate::nav::ReviewScreen;
        use crate::review_state::ReviewTabs;
        let dir = tempfile::tempdir().unwrap();
        let mut app = testing::demo_review_app();
        let pr = crate::fixture::demo_pr();
        let screen = {
            let mut q = app.world_mut().query::<(Entity, &ReviewScreen)>();
            q.iter(app.world())
                .find(|(_, s)| s.0 == pr)
                .map(|(e, _)| e)
                .expect("the review's screen")
        };
        let mine = png_url(1);
        let path = write_png(dir.path(), "m.png", 2, 2);
        let e = app
            .world_mut()
            .spawn((Node::default(), MdImage(mine.clone()), ChildOf(screen)))
            .id();
        app.update();
        deliver(&mut app, &mine, Ok(file(&path, MediaKind::Png)));
        assert!(app.world().get::<ImageNode>(e).is_some());
        let elsewhere = png_url(2);
        load_png(&mut app, dir.path(), &elsewhere);
        assert!(cached(&app).contains(&mine));
        app.world_mut().despawn(e);
        app.world_mut().resource_mut::<ReviewTabs>().0.remove(&pr);
        app.update();
        let urls = cached(&app);
        assert!(!urls.contains(&mine), "the closed tab's picture is gone");
        assert!(urls.contains(&elsewhere), "a picture of no tab stays");
    }
}
