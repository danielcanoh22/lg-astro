//! Installed-app artwork, discovered and decoded on its own worker thread.
//!
//! Icons are resolved from inventory metadata and each app's `appinfo.json` inside the known
//! application roots. When the jailed app cannot read another app's files, the native
//! `astro-helper` reads them through [`crate::platform::read_app_icons`]. The UI requests visible
//! cards first and may prefetch the rest; finished pixels are uploaded to the GPU on the SDL
//! thread. Decoded artwork is kept in a bounded LRU cache keyed by [`Source`].
use crate::model::valid_app_id;
use image::{ImageReader, Limits, RgbaImage};
use serde::Serialize;
use serde_json::Value;
use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    io::{Cursor, Read},
    path::{Component, Path, PathBuf},
    sync::{Arc, Condvar, Mutex, mpsc},
    thread,
};

/// Application install roots on webOS. Icon paths outside these are never read.
const ROOTS: [&str; 5] = [
    "/media/developer/apps/usr/palm/applications",
    "/media/cryptofs/apps/usr/palm/applications",
    "/usr/palm/applications",
    "/media/system/apps/usr/palm/applications",
    "/mnt/otncabi/usr/palm/applications",
];
/// Largest icon file read, in bytes, locally or through the elevated helper.
pub const MAX_FILE: usize = 512 * 1024;
/// Decoded pixel bytes kept in the cache before least-recently-used entries are evicted.
const CACHE_BYTES: usize = 16 * 1024 * 1024;
/// Cache entries kept before least-recently-used entries are evicted.
const CACHE_ENTRIES: usize = 256;
/// Requests queued for the worker at once; further requests are refused until replies drain.
const REQUEST_SLOTS: usize = 16;
// Prefetched icons in flight at once: the rest of the request queue stays free for visible cards.
const PREFETCH_SLOTS: usize = 8;

/// Where to find one app's icon, taken from inventory metadata.
///
/// It is also the cache key and is serialized as the request to the elevated icon reader.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct Source {
    pub id: String,
    directory: Option<PathBuf>,
    large_icon: Option<String>,
    icon: Option<String>,
    // Part of the cache key: an app update changes it, so its icon is read
    // again, while unchanged apps keep their decoded artwork across refreshes.
    version: Option<String>,
}

impl Source {
    /// Builds a source from an app's inventory metadata.
    ///
    /// Empty, overlong or control-character values are ignored, and `folderPath` is kept only
    /// when it names a direct child of one of the application roots.
    pub fn new(id: &str, metadata: &Value) -> Self {
        let text = |key| {
            metadata
                .get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty() && s.len() <= 4096 && !s.chars().any(char::is_control))
        };
        Self {
            id: id.into(),
            directory: text("folderPath").map(PathBuf::from).filter(|p| {
                p.components().all(|c| !matches!(c, Component::ParentDir))
                    && ROOTS.iter().any(|root| p.parent() == Some(Path::new(root)))
            }),
            large_icon: text("largeIcon").map(str::to_owned),
            icon: text("icon").map(str::to_owned),
            version: text("version").map(str::to_owned),
        }
    }

    /// Returns the reported folder first, then the app ID under every application root.
    fn directories(&self) -> Vec<PathBuf> {
        let mut directories: Vec<_> = self.directory.iter().cloned().collect();
        for root in ROOTS {
            let directory = Path::new(root).join(&self.id);
            if !directories.contains(&directory) {
                directories.push(directory);
            }
        }
        directories
    }
}

/// A decoded icon ready for upload, with the card colours chosen for it.
pub struct Artwork {
    /// Texture cache key, unique per request so a re-read icon never reuses a stale texture.
    pub key: String,
    /// At most 256 × 256 pixels, with transparent pixels bled for clean GPU filtering.
    pub pixels: RgbaImage,
    /// The card behind the icon: top and bottom of a vertical gradient (equal when flat).
    pub backdrop: [[u8; 3]; 2],
}

impl Artwork {
    /// Returns whether a dark badge contrasts better than a light one at height `y` of the card.
    ///
    /// `y` is a fraction of the card height (clamped to 0..=1). The badge sits outside the inset
    /// icon, directly over the card gradient, so only the backdrop decides.
    pub fn dark_badge(&self, y: f32) -> bool {
        let y = y.clamp(0., 1.);
        let rgb = std::array::from_fn(|i| {
            (self.backdrop[0][i] as f32 * (1. - y) + self.backdrop[1][i] as f32 * y).round() as u8
        });
        let background = luminance(rgb);
        contrast(0., background) > contrast(1., background)
    }
}

/// Reads at most `limit` bytes of a file inside the application roots.
fn read_bounded(path: &Path, limit: usize) -> Option<Vec<u8>> {
    read_within(path, limit, &ROOTS.map(PathBuf::from))
}

/// Reads a regular file of at most `limit` bytes that resolves inside one of `roots`.
///
/// As Home this runs as root, so a link planted in an application's directory must not
/// select a file outside the application roots.
fn read_within(path: &Path, limit: usize, roots: &[PathBuf]) -> Option<Vec<u8>> {
    let file = astro_tv_helper::open_within(path, roots)?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > limit as u64 {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() <= limit).then_some(bytes)
}

/// Resolves a manifest icon value against the app directory.
///
/// Accepts relative paths, absolute paths and `file://` URLs; rejects other URL schemes, `..`
/// components and anything that ends up outside the application roots.
fn icon_path(directory: &Path, value: &str) -> Option<PathBuf> {
    if value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control) {
        return None;
    }
    let value = value.strip_prefix("file://").unwrap_or(value);
    if value.contains("://") {
        return None;
    }
    let path = Path::new(value);
    if path
        .components()
        .any(|part| matches!(part, Component::ParentDir))
    {
        return None;
    }
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        directory.join(path)
    };
    ROOTS
        .iter()
        .any(|root| path.starts_with(root))
        .then_some(path)
}

/// Decodes an icon of at most 2048 × 2048 pixels and picks its card backdrop.
///
/// The returned [`Artwork::key`] is empty; the worker fills it in per request.
fn decode(bytes: &[u8]) -> Option<Artwork> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(2048);
    limits.max_image_height = Some(2048);
    limits.max_alloc = Some(32 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode().ok()?;
    let pixels = thumbnail(image.into_rgba8());
    let backdrop = backdrop(&pixels);
    Some(Artwork {
        key: String::new(),
        pixels,
        backdrop,
    })
}

/// Scales with premultiplied alpha, so the colour of transparent pixels (often black) never
/// darkens antialiased edges. Transparent texels then take their neighbours' colour, which
/// the GPU's linear filter blends in at the logo's edge.
fn thumbnail(mut image: RgbaImage) -> RgbaImage {
    for pixel in image.pixels_mut() {
        let alpha = pixel[3] as u32;
        for channel in 0..3 {
            pixel[channel] = ((pixel[channel] as u32 * alpha + 127) / 255) as u8;
        }
    }
    let mut pixels = image::DynamicImage::ImageRgba8(image)
        .thumbnail(256, 256)
        .into_rgba8();
    for pixel in pixels.pixels_mut() {
        let alpha = pixel[3] as u32;
        if alpha > 0 {
            for channel in 0..3 {
                pixel[channel] = ((pixel[channel] as u32 * 255 + alpha / 2) / alpha).min(255) as u8;
            }
        }
    }
    bleed(&mut pixels, 6);
    pixels
}

/// Gives fully transparent pixels the average colour of coloured neighbours, `passes` deep.
fn bleed(pixels: &mut RgbaImage, passes: usize) {
    let (width, height) = pixels.dimensions();
    let mut coloured: Vec<bool> = pixels.pixels().map(|pixel| pixel[3] > 0).collect();
    for _ in 0..passes {
        let source = pixels.clone();
        let known = coloured.clone();
        for y in 0..height {
            for x in 0..width {
                if known[(y * width + x) as usize] {
                    continue;
                }
                let (mut count, mut sum) = (0, [0u32; 3]);
                for ny in y.saturating_sub(1)..(y + 2).min(height) {
                    for nx in x.saturating_sub(1)..(x + 2).min(width) {
                        if known[(ny * width + nx) as usize] {
                            let pixel = source.get_pixel(nx, ny);
                            count += 1;
                            for channel in 0..3 {
                                sum[channel] += pixel[channel] as u32;
                            }
                        }
                    }
                }
                if count > 0 {
                    let pixel = pixels.get_pixel_mut(x, y);
                    for channel in 0..3 {
                        pixel[channel] = (sum[channel] / count) as u8;
                    }
                    coloured[(y * width + x) as usize] = true;
                }
            }
        }
    }
}

/// Chooses the card colours behind an icon: its dominant opaque edge colour, or a generated
/// gradient for transparent artwork.
fn backdrop(image: &RgbaImage) -> [[u8; 3]; 2] {
    // Match the icon's opaque edges rather than letting its central logo choose
    // the card colour. Transparent artwork gets a gradient of its own instead, even
    // when its logo reaches into the edge band: those few opaque pixels would
    // otherwise fill the card with the logo's own colour and hide it.
    let sample = image::imageops::resize(image, 48, 48, image::imageops::FilterType::Triangle);
    let mut bins = HashMap::<[u8; 3], (usize, [usize; 3])>::new();
    let (mut edge, mut opaque) = (0, 0);
    for (x, y, pixel) in sample.enumerate_pixels() {
        if (6..42).contains(&x) && (6..42).contains(&y) {
            continue;
        }
        edge += 1;
        if pixel[3] < 230 {
            continue;
        }
        opaque += 1;
        let bin = bins
            .entry([pixel[0] >> 4, pixel[1] >> 4, pixel[2] >> 4])
            .or_default();
        bin.0 += 1;
        for channel in 0..3 {
            bin.1[channel] += pixel[channel] as usize;
        }
    }
    if opaque * 2 < edge {
        return logo_backdrop(image);
    }
    bins.into_iter()
        .max_by_key(|(key, (count, _))| (*count, *key))
        .map_or_else(
            || logo_backdrop(image),
            |(_, (count, sum))| {
                let rgb = sum.map(|s| ((s + count / 2) / count) as u8);
                [rgb, rgb]
            },
        )
}

/// Transparent artwork: a gradient in the logo's dominant hue, light behind a dark logo and
/// dark behind a light one, with at least 3:1 contrast against the logo's average luminance.
/// Logos without a clear hue get a neutral slate.
fn logo_backdrop(image: &RgbaImage) -> [[u8; 3]; 2] {
    let (mut weight, mut luminance_sum, mut saturated_weight, mut saturation_sum) =
        (0., 0., 0., 0.);
    let mut hues = [(0f32, 0f32, 0f32); 24];
    for pixel in image.pixels() {
        let alpha = pixel[3] as f32 / 255.;
        if alpha < 0.25 {
            continue;
        }
        let rgb = [pixel[0], pixel[1], pixel[2]];
        weight += alpha;
        luminance_sum += alpha * luminance(rgb);
        let (hue, saturation, value) = hsv(rgb);
        if saturation >= 0.3 && value >= 0.2 {
            let bin = &mut hues[(hue / 15.) as usize % 24];
            let strength = alpha * saturation;
            bin.0 += strength;
            bin.1 += strength * hue.to_radians().cos();
            bin.2 += strength * hue.to_radians().sin();
            saturated_weight += alpha;
            saturation_sum += alpha * saturation;
        }
    }
    let logo = if weight > 0. {
        luminance_sum / weight
    } else {
        1.
    };
    let (hue, saturation) = if weight > 0. && saturated_weight >= 0.15 * weight {
        let (_, x, y) = hues
            .into_iter()
            .max_by(|a, b| a.0.total_cmp(&b.0))
            .unwrap_or_default();
        (
            y.atan2(x).to_degrees().rem_euclid(360.),
            (saturation_sum / saturated_weight).min(0.6),
        )
    } else {
        (220., 0.12)
    };
    let gradient = |top: f32, saturation: f32| {
        // Move both stops away from the logo's lightness until each has enough contrast.
        let step = if top > 0.5 { 0.02 } else { -0.02 };
        let mut top = top;
        loop {
            let stops = [hsl(hue, saturation, top), hsl(hue, saturation, top - 0.1)];
            let worst = stops
                .iter()
                .map(|stop| contrast(luminance(*stop), logo))
                .fold(f32::MAX, f32::min);
            if worst >= 3. || !(0.14..=0.98).contains(&(top + step)) {
                return (worst, stops);
            }
            top += step;
        }
    };
    let light = gradient(0.9, saturation * 0.8);
    let dark = gradient(0.27, saturation);
    if light.0 > dark.0 { light.1 } else { dark.1 }
}

/// Returns the WCAG relative luminance of an sRGB colour.
fn luminance(rgb: [u8; 3]) -> f32 {
    let linear = |c: u8| {
        let c = c as f32 / 255.;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(rgb[0]) + 0.7152 * linear(rgb[1]) + 0.0722 * linear(rgb[2])
}

/// Returns the WCAG contrast ratio between two relative luminances.
fn contrast(a: f32, b: f32) -> f32 {
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// Hue in degrees, saturation and value in 0..1.
fn hsv(rgb: [u8; 3]) -> (f32, f32, f32) {
    let [r, g, b] = rgb.map(|c| c as f32 / 255.);
    let max = r.max(g).max(b);
    let range = max - r.min(g).min(b);
    let hue = if range == 0. {
        0.
    } else if max == r {
        60. * ((g - b) / range).rem_euclid(6.)
    } else if max == g {
        60. * ((b - r) / range + 2.)
    } else {
        60. * ((r - g) / range + 4.)
    };
    (hue, if max == 0. { 0. } else { range / max }, max)
}

/// Converts hue in degrees and saturation and lightness in 0..1 to sRGB.
fn hsl(hue: f32, saturation: f32, lightness: f32) -> [u8; 3] {
    let chroma = (1. - (2. * lightness - 1.).abs()) * saturation;
    let x = chroma * (1. - ((hue / 60.).rem_euclid(2.) - 1.).abs());
    let (r, g, b) = match (hue / 60.) as u32 {
        0 => (chroma, x, 0.),
        1 => (x, chroma, 0.),
        2 => (0., chroma, x),
        3 => (0., x, chroma),
        4 => (x, 0., chroma),
        _ => (chroma, 0., x),
    };
    let m = lightness - chroma / 2.;
    [r, g, b].map(|c| ((c + m).clamp(0., 1.) * 255.).round() as u8)
}

/// Loads one app's icon from its installed files, falling back to the elevated helper.
fn load(source: &Source) -> Option<Artwork> {
    if !valid_app_id(&source.id) {
        return None;
    }
    if let Some(image) = load_local(source, read_bounded) {
        return Some(image);
    }
    // Some jailed installations cannot read other applications' files. Use the
    // same native-authenticated elevated service as inventory, on this worker.
    match crate::platform::read_app_icons(source) {
        Ok(icons) => icons.iter().find_map(|bytes| decode(bytes)),
        Err(error) => {
            eprintln!("Astro installed icon {}: {error}", source.id);
            None
        }
    }
}

/// Tries each candidate directory and icon in preference order and returns the first that
/// decodes.
///
/// Inventory icons come before the manifest's, large icons before small, and `icon.png` is the
/// last resort. A manifest whose `id` differs from the app's is ignored.
fn load_local(
    source: &Source,
    mut read: impl FnMut(&Path, usize) -> Option<Vec<u8>>,
) -> Option<Artwork> {
    for directory in source.directories() {
        let manifest = read(&directory.join("appinfo.json"), 64 * 1024)
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .filter(|value| value.get("id").and_then(Value::as_str) == Some(&source.id));
        let manifest_icon = |key| {
            manifest
                .as_ref()
                .and_then(|m| m.get(key))
                .and_then(Value::as_str)
        };
        for icon in [
            source.large_icon.as_deref(),
            manifest_icon("largeIcon"),
            source.icon.as_deref(),
            manifest_icon("icon"),
            Some("icon.png"),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(artwork) = icon_path(&directory, icon)
                .and_then(|path| read(&path, MAX_FILE))
                .and_then(|bytes| decode(&bytes))
            {
                return Some(artwork);
            }
        }
    }
    None
}

/// A queued icon read; `token` matches the reply to the cache entry that asked for it.
struct Request {
    token: u64,
    source: Source,
}

/// The worker's two-level queue: cards on screen are served before prefetches.
#[derive(Default)]
struct Requests {
    visible: VecDeque<Request>,
    background: VecDeque<Request>,
    closed: bool,
}

/// The UI side of the request queue. Dropping it closes the queue and stops the worker.
struct RequestSender {
    queue: Arc<(Mutex<Requests>, Condvar)>,
}

impl RequestSender {
    fn try_send(&self, request: Request, background: bool) -> Result<(), ()> {
        let (mutex, ready) = &*self.queue;
        let mut queue = mutex.lock().unwrap();
        if queue.closed || queue.visible.len() + queue.background.len() >= REQUEST_SLOTS {
            return Err(());
        }
        if background {
            queue.background.push_back(request);
        } else {
            queue.visible.push_back(request);
        }
        ready.notify_one();
        Ok(())
    }

    /// Moves a queued prefetch to the visible queue because its card is now on screen.
    fn promote(&self, token: u64) {
        let mut queue = self.queue.0.lock().unwrap();
        if let Some(index) = queue.background.iter().position(|r| r.token == token) {
            let request = queue.background.remove(index).unwrap();
            queue.visible.push_back(request);
        }
    }

    fn clear(&self) {
        let mut queue = self.queue.0.lock().unwrap();
        queue.visible.clear();
        queue.background.clear();
    }
}

impl Drop for RequestSender {
    fn drop(&mut self) {
        let (mutex, ready) = &*self.queue;
        mutex.lock().unwrap().closed = true;
        ready.notify_one();
    }
}

struct Reply {
    request: Request,
    image: Option<Arc<Artwork>>,
}
struct Entry {
    token: u64,
    used: u64,
    // None: pending; Some(None): unavailable until the next inventory refresh.
    image: Option<Option<Arc<Artwork>>>,
}
#[derive(Default)]
struct Cache {
    clock: u64,
    entries: HashMap<Source, Entry>,
}

/// The UI's handle to the artwork worker and its cache. Use it only from the SDL thread.
pub struct Service {
    sender: Option<RequestSender>,
    receiver: mpsc::Receiver<Reply>,
    cache: RefCell<Cache>,
    prefetch: RefCell<VecDeque<Source>>,
}

impl Service {
    /// Starts the artwork worker, or returns an inert service when `enabled` is false.
    pub fn new(enabled: bool) -> Self {
        Self::with_loader(enabled, load)
    }

    fn with_loader(
        enabled: bool,
        loader: impl Fn(&Source) -> Option<Artwork> + Send + 'static,
    ) -> Self {
        let queue = Arc::new((Mutex::new(Requests::default()), Condvar::new()));
        // Room for a full request queue: an idle UI only polls on its next
        // wake, and a one-slot reply channel held the worker to one icon each.
        let (responses, receiver) = mpsc::sync_channel(16);
        if enabled {
            let queue = queue.clone();
            thread::spawn(move || {
                loop {
                    let request = {
                        let (mutex, ready) = &*queue;
                        let mut queue = mutex.lock().unwrap();
                        loop {
                            if queue.closed {
                                return;
                            }
                            // Visible requests, including promoted prefetches, always go next.
                            if let Some(request) = queue
                                .visible
                                .pop_front()
                                .or_else(|| queue.background.pop_front())
                            {
                                break request;
                            }
                            queue = ready.wait(queue).unwrap();
                        }
                    };
                    let image = loader(&request.source).map(|mut image| {
                        image.key = format!("installed-icon:{}", request.token);
                        Arc::new(image)
                    });
                    if responses.send(Reply { request, image }).is_err() {
                        break;
                    }
                }
            });
        }
        Self {
            sender: enabled.then_some(RequestSender { queue }),
            receiver,
            cache: RefCell::default(),
            prefetch: RefCell::default(),
        }
    }

    /// Returns the cached artwork for a visible card, queueing a read when it is not cached.
    ///
    /// `None` means not loaded yet, unavailable, or the queue is full; the card shows its
    /// fallback meanwhile.
    pub fn get(&self, source: &Source) -> Option<Arc<Artwork>> {
        self.request(source, false).ok().flatten()
    }

    /// Returns the cached artwork, or queues a request for it and returns `Ok(None)`.
    ///
    /// `Err` when the request queue is full: a visible card is retried when the next completed
    /// request redraws the scene, so neither scrolling nor a slow Homebrew service can block
    /// input.
    fn request(&self, source: &Source, background: bool) -> Result<Option<Arc<Artwork>>, ()> {
        let sender = self.sender.as_ref().ok_or(())?;
        let mut cache = self.cache.borrow_mut();
        cache.clock += 1;
        let clock = cache.clock;
        if let Some(entry) = cache.entries.get_mut(source) {
            entry.used = clock;
            if !background && entry.image.is_none() {
                sender.promote(entry.token);
            }
            return Ok(entry.image.clone().flatten());
        }
        let request = Request {
            token: clock,
            source: source.clone(),
        };
        sender.try_send(request, background)?;
        cache.entries.insert(
            source.clone(),
            Entry {
                token: clock,
                used: clock,
                image: None,
            },
        );
        Ok(None)
    }

    /// Decodes these icons in the background, such as every installed app, so pages open with
    /// their artwork ready. Replaces any earlier prefetch list; icons are fed to the worker a
    /// few at a time as replies come in.
    pub fn prefetch(&self, sources: impl IntoIterator<Item = Source>) {
        if self.sender.is_none() {
            return;
        }
        *self.prefetch.borrow_mut() = sources.into_iter().collect();
        self.feed();
    }

    /// Queues prefetches until [`PREFETCH_SLOTS`] requests are in flight.
    fn feed(&self) {
        let mut queue = self.prefetch.borrow_mut();
        let mut in_flight = self.in_flight();
        while in_flight < PREFETCH_SLOTS
            && let Some(source) = queue.front()
        {
            let cached = self.cache.borrow().entries.contains_key(source);
            if !cached {
                if self.request(source, true).is_err() {
                    break;
                }
                in_flight += 1;
            }
            queue.pop_front();
        }
    }

    fn in_flight(&self) -> usize {
        self.cache
            .borrow()
            .entries
            .values()
            .filter(|entry| entry.image.is_none())
            .count()
    }

    /// Returns whether a requested icon has not come back yet, so the event loop should wake
    /// soon to poll for it.
    pub fn pending(&self) -> bool {
        self.in_flight() > 0 || !self.prefetch.borrow().is_empty()
    }

    /// Forgets every icon so it is read again, for the manual app refresh.
    ///
    /// Replies to earlier requests no longer match an entry's token and are dropped.
    pub fn reset(&self) {
        self.cache.borrow_mut().entries.clear();
        self.prefetch.borrow_mut().clear();
        if let Some(sender) = &self.sender {
            sender.clear();
        }
    }

    /// Retries icons that were unavailable, after an automatic inventory refresh.
    ///
    /// Decoded and pending icons stay, because a [`Source`] names the app's version and icon
    /// paths, so an updated app gets a new cache entry.
    pub fn refresh(&self) {
        self.cache
            .borrow_mut()
            .entries
            .retain(|_, entry| !matches!(entry.image, Some(None)));
    }

    /// Collects finished replies, evicts least-recently-used artwork over the cache limits and
    /// queues more prefetches. Returns whether any reply arrived, so the scene needs a redraw.
    pub fn poll(&self) -> bool {
        let mut changed = false;
        let mut cache = self.cache.borrow_mut();
        while let Ok(reply) = self.receiver.try_recv() {
            changed = true;
            if let Some(entry) = cache.entries.get_mut(&reply.request.source)
                && entry.token == reply.request.token
            {
                entry.image = Some(reply.image);
            }
        }
        if !changed {
            return false;
        }
        let mut bytes: usize = cache
            .entries
            .values()
            .filter_map(|e| e.image.as_ref()?.as_ref())
            .map(|i| i.pixels.as_raw().len())
            .sum();
        let mut old: Vec<_> = cache
            .entries
            .iter()
            .filter(|(_, e)| e.image.is_some())
            .map(|(s, e)| (s.clone(), e.used))
            .collect();
        old.sort_by_key(|(_, used)| *used);
        for (source, _) in old {
            if bytes <= CACHE_BYTES && cache.entries.len() <= CACHE_ENTRIES {
                break;
            }
            if let Some(entry) = cache.entries.remove(&source)
                && let Some(Some(image)) = entry.image
            {
                bytes -= image.pixels.as_raw().len();
            }
        }
        drop(cache);
        self.feed();
        changed
    }
}

#[cfg(test)]
#[path = "app-artwork-tests.rs"]
mod tests;
