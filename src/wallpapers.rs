//! Immutable, app-owned wallpapers and a short-lived, capability-protected LAN upload server.
//! Network I/O, decoding, normalization, metadata discovery and QR generation stay on this worker.
pub use crate::companion::Session;
use crate::companion::{
    PENDING_LIMIT, Pending, Request, carries_token, error_response, listen, local_peer,
    random_token, read_exact, redirect_to_page, response, session,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{BufReader, Cursor, Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

const MAX_BODY: usize = 12 * 1024 * 1024;
const MAX_WALLPAPERS: usize = 20;
const SESSION_TIME: Duration = Duration::from_secs(600);
/// No upload outlives its session, so staging this old belongs to no live process.
const STAGING_ABANDONED: Duration = Duration::from_secs(SESSION_TIME.as_secs() + 5 * 60);
const SESSION_REQUEST_LIMIT: usize = 64;
const REQUEST_TIME: Duration = Duration::from_secs(20);
const STOP: u64 = u64::MAX;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Wallpaper {
    pub id: String,
    #[serde(skip)]
    pub directory: PathBuf,
    #[serde(skip)]
    pub has_original: bool,
}

impl Wallpaper {
    pub fn unshaded_image(&self) -> String {
        // Preserve full resolution for older uploads without an unshaded original.
        self.directory
            .join(if self.has_original {
                "original.jpg"
            } else {
                "wallpaper.jpg"
            })
            .to_string_lossy()
            .into_owned()
    }
    pub fn image(&self) -> String {
        self.directory
            .join("wallpaper.jpg")
            .to_string_lossy()
            .into_owned()
    }
    pub fn thumbnail(&self) -> String {
        self.directory
            .join("thumbnail.jpg")
            .to_string_lossy()
            .into_owned()
    }
}

pub fn valid_id(id: &str) -> bool {
    id.strip_prefix("custom-").is_some_and(|id| {
        id.len() == 32
            && id
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    })
}

pub enum Event {
    Catalog(Vec<Wallpaper>),
    Deleted(String, Result<(), String>),
    Session(u64, Result<Session, String>),
    Ended(u64),
}

enum Command {
    Wake,
    Delete { id: String, selected: String },
}

pub struct Service {
    desired: Arc<AtomicU64>,
    wake: mpsc::Sender<Command>,
    pub receiver: mpsc::Receiver<Event>,
    active: bool,
    generation: u64,
}

impl Service {
    pub fn new(preferences: &Path, loopback_only: bool) -> Result<Self, String> {
        let directory = preferences
            .parent()
            .unwrap_or(Path::new("."))
            .join("wallpapers");
        let desired = Arc::new(AtomicU64::new(0));
        let worker_desired = desired.clone();
        let (sender, receiver) = mpsc::sync_channel(4);
        let (wake, updates) = mpsc::channel();
        thread::Builder::new()
            .name("astro-wallpapers".into())
            .spawn(move || {
                worker(
                    &directory,
                    &worker_desired,
                    &sender,
                    &updates,
                    loopback_only,
                );
            })
            .map_err(|e| format!("Could not start wallpaper worker: {e}"))?;
        Ok(Self {
            desired,
            wake,
            receiver,
            active: false,
            generation: 0,
        })
    }

    pub fn set_active(&mut self, active: bool) {
        if self.active != active {
            self.active = active;
            self.generation += 1;
            self.desired
                .store(if active { self.generation } else { 0 }, Ordering::Release);
            let _ = self.wake.send(Command::Wake);
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn delete(&self, id: String, selected: String) {
        let _ = self.wake.send(Command::Delete { id, selected });
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.desired.store(STOP, Ordering::Release);
        let _ = self.wake.send(Command::Wake);
    }
}

fn random_id() -> Result<String, String> {
    let mut bytes = [0; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| format!("Secure random source unavailable: {e}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn real_directory(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

fn small_file(path: &Path, maximum: u64) -> Option<Vec<u8>> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > maximum {
        return None;
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(maximum + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= maximum).then_some(bytes)
}

fn catalog(directory: &Path) -> Vec<Wallpaper> {
    if !real_directory(directory) {
        return vec![];
    }
    let Ok(entries) = fs::read_dir(directory) else {
        return vec![];
    };
    let mut result = Vec::new();
    let candidates = entries.flatten().filter_map(|entry| {
        let id = entry.file_name().to_string_lossy().into_owned();
        valid_id(&id).then(|| (entry.path(), id))
    });
    // Bound discovery even if someone manually populates this private directory. Only
    // wallpaper-shaped names count, so unrelated entries cannot hide real wallpapers.
    for (path, id) in candidates.take(128) {
        if !real_directory(&path) {
            continue;
        }
        let Some(bytes) = small_file(&path.join("metadata.json"), 2048) else {
            continue;
        };
        let Ok(mut item) = serde_json::from_slice::<Wallpaper>(&bytes) else {
            continue;
        };
        // Legacy metadata may contain a name; serde ignores it. Identity and
        // selection depend only on the generated ID, never a display label.
        if item.id != id {
            continue;
        }
        if ["wallpaper.jpg", "thumbnail.jpg"].iter().any(|file| {
            !fs::symlink_metadata(path.join(file))
                .is_ok_and(|m| m.is_file() && m.len() > 0 && m.len() <= MAX_BODY as u64)
        }) {
            continue;
        }
        item.has_original = fs::symlink_metadata(path.join("original.jpg"))
            .is_ok_and(|m| m.is_file() && m.len() > 0 && m.len() <= MAX_BODY as u64);
        item.directory = path;
        result.push(item);
        if result.len() == MAX_WALLPAPERS {
            break;
        }
    }
    result.sort_by(|a, b| a.id.cmp(&b.id));
    result
}

fn delete_wallpaper(directory: &Path, id: &str, selected: &str) -> Result<(), String> {
    if id == selected || !valid_id(id) || !real_directory(directory) {
        return Err("Background cannot be deleted".into());
    }
    let path = directory.join(id);
    if !real_directory(&path) {
        return Err("Background directory unavailable".into());
    }
    // Only the strictly validated app-owned directory is removed; symlink
    // entries within it are unlinked, never followed by remove_dir_all.
    fs::remove_dir_all(path).map_err(|error| error.to_string())
}

/// Removes `.custom-<id>` staging entries an interrupted upload left behind (power loss or a
/// kill before the commit rename). Normal and takeover launches share this gallery and may run
/// at once, so only entries last modified more than `STAGING_ABANDONED` ago are removed; an
/// unreadable or future mtime keeps the entry until a later start. Symlinks are unlinked,
/// never followed; real directories are removed like deleted wallpapers. Every other entry,
/// and a symlinked root, is left alone.
fn remove_interrupted_uploads(directory: &Path) {
    if !real_directory(directory) {
        return;
    }
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => {
            eprintln!("Could not scan wallpaper storage for interrupted uploads: {error}");
            return;
        }
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name
            .to_str()
            .and_then(|name| name.strip_prefix('.'))
            .is_some_and(valid_id)
        {
            continue;
        }
        let path = entry.path();
        let abandoned = |metadata: &fs::Metadata| {
            metadata
                .modified()
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age > STAGING_ABANDONED)
        };
        let removed = match fs::symlink_metadata(&path) {
            Ok(metadata) if !abandoned(&metadata) => continue,
            Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(&path),
            Ok(_) => fs::remove_file(&path),
            Err(error) => Err(error),
        };
        if let Err(error) = removed {
            eprintln!(
                "Could not remove interrupted wallpaper upload {}: {error}",
                path.display()
            );
        }
    }
}

/// Persists a directory's entries (new files, or a rename inside it) across power loss.
fn sync_directory(directory: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    fs::File::open(directory)?.sync_all()?;
    let _ = directory;
    Ok(())
}

/// Root Home creates files readable by the same app user after takeover is disabled.
fn create_directory(path: &Path) -> Result<(), String> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => inherit_owner(path)?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && real_directory(path) => {
        }
        Err(error) => return Err(error.to_string()),
    }
    if !real_directory(path) {
        return Err("Wallpaper storage is not a real directory".into());
    }
    Ok(())
}

fn inherit_owner(path: &Path) -> Result<(), String> {
    #[cfg(all(feature = "webos", unix))]
    if unsafe { libc::geteuid() } == 0 {
        use std::os::{fd::AsRawFd, unix::fs::MetadataExt};
        let parent = path.parent().ok_or("Missing wallpaper storage parent")?;
        let owner = fs::symlink_metadata(parent).map_err(|e| e.to_string())?;
        if !owner.is_dir() {
            return Err("Invalid wallpaper storage parent".into());
        }
        use std::os::unix::fs::OpenOptionsExt;
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(|e| e.to_string())?;
        if unsafe { libc::fchown(file.as_raw_fd(), owner.uid(), owner.gid()) } != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
    }
    let _ = path;
    Ok(())
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|e| e.to_string())?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| e.to_string())?;
    inherit_owner(path)
}

fn decode(bytes: &[u8]) -> Result<image::RgbImage, String> {
    if bytes.len() > MAX_BODY {
        return Err("Image exceeds 12 MiB".into());
    }
    let format = image::guess_format(bytes).map_err(|_| "Choose a JPEG, PNG or WebP image")?;
    if !matches!(
        format,
        image::ImageFormat::Jpeg | image::ImageFormat::Png | image::ImageFormat::WebP
    ) {
        return Err("Choose a JPEG, PNG or WebP image".into());
    }
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(1920);
    limits.max_image_height = Some(1080);
    limits.max_alloc = Some(32 * 1024 * 1024);
    reader.limits(limits);
    let mut decoder = reader
        .into_decoder()
        .map_err(|e| format!("Invalid image: {e}"))?;
    use image::ImageDecoder;
    if decoder.dimensions() != (1920, 1080) {
        return Err("Crop the image to 1920 × 1080 before uploading".into());
    }
    let orientation = decoder.orientation().map_err(|e| e.to_string())?;
    let mut image = image::DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    image.apply_orientation(orientation);
    if image.width() != 1920 || image.height() != 1080 {
        return Err("Unsupported image orientation".into());
    }
    // Opaque images (every JPEG the upload page sends) convert in place, without the RGBA
    // copy and per-pixel compositing that only translucent images need.
    if !image.color().has_alpha() {
        return Ok(image.into_rgb8());
    }
    // Composite alpha against the launcher's base instead of exposing transparent pixels as black.
    let rgba = image.into_rgba8();
    Ok(image::RgbImage::from_fn(1920, 1080, |x, y| {
        let p = rgba.get_pixel(x, y).0;
        let alpha = p[3] as f32 / 255.;
        image::Rgb(std::array::from_fn(|i| {
            (p[i] as f32 * alpha + [23., 25., 28.][i] * (1. - alpha)).round() as u8
        }))
    }))
}

fn shade(image: &mut image::RgbImage) {
    // Same base, image opacity and gradients as tools/assets.mjs. Only source-specific
    // saturation adjustments are omitted for owner-supplied photographs.
    let stops = [
        (0., [13., 16., 19.], 0.24),
        (0.19, [14., 17., 21.], 0.08),
        (0.35, [15., 17., 20.], 0.35),
        (0.63, [17., 19., 23.], 0.86),
        (0.85, [23., 25., 29.], 0.92157),
        (1., [23., 25., 29.], 0.96078),
    ];
    // The vertical gradient depends only on the row and the horizontal one only on the
    // column, so both are computed once per row/column with the same arithmetic.
    let rows: Vec<(f32, [f32; 3])> = (0..image.height())
        .map(|y| {
            let y = y as f32 / 1079.;
            let pair = stops
                .windows(2)
                .find(|s| y <= s[1].0)
                .unwrap_or(&stops[4..]);
            let t = (y - pair[0].0) / (pair[1].0 - pair[0].0);
            let alpha = pair[0].2 * (1. - t) + pair[1].2 * t;
            // SVG gradients interpolate premultiplied colors.
            let gradient = std::array::from_fn(|i| {
                pair[0].1[i] * pair[0].2 * (1. - t) + pair[1].1[i] * pair[1].2 * t
            });
            (alpha, gradient)
        })
        .collect();
    let columns: Vec<f32> = (0..image.width())
        .map(|x| 0.42 * (1. - x as f32 / (1919. * 0.75)).max(0.))
        .collect();
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        let (alpha, gradient) = rows[y as usize];
        let horizontal = columns[x as usize];
        for i in 0..3 {
            let base = pixel[i] as f32 * 0.92 + [23., 25., 28.][i] * 0.08;
            let base = base * (1. - horizontal) + [17., 20., 23.][i] * horizontal;
            pixel[i] = (base * (1. - alpha) + gradient[i]).round() as u8;
        }
    }
}

/// The 480 × 270 gallery thumbnail of a 1920 × 1080 image: each pixel is the rounded average
/// of its 4 × 4 source block, with no intermediate full-size buffer.
fn thumbnail(image: &image::RgbImage) -> image::RgbImage {
    image::RgbImage::from_fn(480, 270, |x, y| {
        let mut sum = [0u32; 3];
        for dy in 0..4 {
            for dx in 0..4 {
                let p = image.get_pixel(x * 4 + dx, y * 4 + dy).0;
                for i in 0..3 {
                    sum[i] += p[i] as u32;
                }
            }
        }
        image::Rgb(sum.map(|channel| ((channel + 8) / 16) as u8))
    })
}

fn jpeg(image: &image::RgbImage, quality: u8) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, quality)
        .encode_image(image)
        .map_err(|e| e.to_string())?;
    Ok(bytes)
}

fn store(directory: &Path, bytes: &[u8], valid: impl Fn() -> bool) -> Result<Wallpaper, String> {
    if catalog(directory).len() >= MAX_WALLPAPERS {
        return Err("The wallpaper gallery is full (20 images)".into());
    }
    let mut image = decode(bytes)?;
    let original = jpeg(&image, 94)?;
    let thumbnail = thumbnail(&image);
    shade(&mut image);
    let image = jpeg(&image, 94)?;
    let thumbnail = jpeg(&thumbnail, 82)?;
    if !valid() {
        return Err("Upload session ended".into());
    }
    let parent = directory
        .parent()
        .ok_or("Missing wallpaper directory parent")?;
    if !parent.exists() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    if !real_directory(parent) {
        return Err("Invalid wallpaper storage parent".into());
    }
    create_directory(directory)?;
    let id = format!("custom-{}", random_id()?);
    let staging = directory.join(format!(".{id}"));
    create_directory(&staging)?;
    let item = Wallpaper {
        id: id.clone(),
        directory: directory.join(&id),
        has_original: true,
    };
    let result = (|| {
        write_new(&staging.join("wallpaper.jpg"), &image)?;
        write_new(&staging.join("original.jpg"), &original)?;
        write_new(&staging.join("thumbnail.jpg"), &thumbnail)?;
        write_new(
            &staging.join("metadata.json"),
            &serde_json::to_vec(&item).map_err(|e| e.to_string())?,
        )?;
        if !valid() {
            return Err("Upload session ended".into());
        }
        // The new directory's own entries must be durable before the rename publishes it.
        sync_directory(&staging).map_err(|e| e.to_string())?;
        fs::rename(&staging, &item.directory).map_err(|e| e.to_string())
    })();
    if result.is_err() {
        // Only the files and fresh directory created by this operation.
        for file in [
            "wallpaper.jpg",
            "original.jpg",
            "thumbnail.jpg",
            "metadata.json",
        ] {
            let _ = fs::remove_file(staging.join(file));
        }
        let _ = fs::remove_dir(&staging);
    }
    result?;
    // The rename already committed the upload, so a failed directory sync is only logged.
    if let Err(error) = sync_directory(directory) {
        eprintln!("Could not sync wallpaper storage after an upload: {error}");
    }
    Ok(item)
}

fn worker(
    directory: &Path,
    desired: &AtomicU64,
    sender: &mpsc::SyncSender<Event>,
    updates: &mpsc::Receiver<Command>,
    loopback_only: bool,
) {
    // Clear staging abandoned by an interrupted upload before listing the gallery.
    remove_interrupted_uploads(directory);
    if sender.send(Event::Catalog(catalog(directory))).is_err() {
        return;
    }
    let update = |command| match command {
        Command::Wake => true,
        Command::Delete { id, selected } => {
            let result = delete_wallpaper(directory, &id, &selected);
            sender.send(Event::Deleted(id, result)).is_ok()
                && sender.send(Event::Catalog(catalog(directory))).is_ok()
        }
    };
    let mut handled = 0;
    loop {
        // Finish queued mutations before starting another upload session.
        for command in updates.try_iter() {
            if !update(command) {
                return;
            }
        }
        let generation = desired.load(Ordering::Acquire);
        if generation == STOP {
            break;
        }
        if generation == 0 || generation == handled {
            match updates.recv() {
                Ok(command) => {
                    if !update(command) {
                        break;
                    }
                }
                Err(_) => break,
            }
            continue;
        }
        handled = generation;
        let opened = (|| {
            let listener = listen(loopback_only)?;
            let token = random_token()?;
            let address = listener.local_addr().map_err(|e| e.to_string())?;
            let info = session(address, &token)?;
            Ok::<_, String>((listener, token, address, info))
        })();
        let (listener, token, address, info) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                let _ = sender.send(Event::Session(generation, Err(error)));
                continue;
            }
        };
        if sender.send(Event::Session(generation, Ok(info))).is_err() {
            break;
        }
        let deadline = Instant::now() + SESSION_TIME;
        let valid = || desired.load(Ordering::Acquire) == generation && Instant::now() < deadline;
        let host = address.to_string();
        // Headers from every peer are read side by side, so a slow one cannot hold up the rest.
        let mut pending = Pending::default();
        // Only requests carrying the token count, so other LAN peers cannot use up the session.
        let mut requests = 0;
        'session: while valid() && requests < SESSION_REQUEST_LIMIT {
            let mut idle = true;
            // Serve what arrived before accepting more, so closed connections free their slots.
            for (mut stream, received) in pending.poll() {
                idle = false;
                let received = match received {
                    Ok(received) => received,
                    Err(error) => {
                        if let Err(error) = reject(&mut stream, &error) {
                            eprintln!("Astro wallpaper upload: {error}");
                        }
                        continue;
                    }
                };
                if carries_token(&received, &token) {
                    if requests == SESSION_REQUEST_LIMIT {
                        break;
                    }
                    requests += 1;
                }
                match serve(&mut stream, &received, &host, &token, directory, valid) {
                    Ok(true) => {
                        if sender.send(Event::Catalog(catalog(directory))).is_err() {
                            return;
                        }
                    }
                    Ok(false) => {}
                    Err(error) => eprintln!("Astro wallpaper upload: {error}"),
                }
            }
            for _ in 0..PENDING_LIMIT {
                match listener.accept() {
                    Ok((stream, peer)) => {
                        idle = false;
                        if local_peer(peer.ip()) {
                            pending.admit(stream, peer.ip());
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    // A peer that resets before it is accepted must not end the session.
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::ConnectionAborted
                                | std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::Interrupted
                        ) => {}
                    Err(error) => {
                        eprintln!("Astro wallpaper server: {error}");
                        break 'session;
                    }
                }
            }
            if idle {
                thread::sleep(Duration::from_millis(if pending.is_empty() {
                    25
                } else {
                    10
                }));
            }
        }
        drop(listener);
        if sender.send(Event::Ended(generation)).is_err() {
            break;
        }
    }
}

fn request(
    reader: &mut impl Read,
    valid: &impl Fn() -> bool,
    deadline: Instant,
) -> Result<Request, String> {
    crate::companion::request(reader, valid, deadline, MAX_BODY, "Image exceeds 12 MiB")
}

/// Answers a connection whose headers did not arrive whole or in time.
fn reject(stream: &mut TcpStream, error: &str) -> Result<(), String> {
    stream
        .set_nonblocking(false)
        .and_then(|_| stream.set_write_timeout(Some(Duration::from_secs(2))))
        .map_err(|e| e.to_string())?;
    error_response(stream, "400 Bad Request", error)
}

/// `received` holds bytes already read from `stream`: the complete headers and possibly the
/// start of the body. The body gets its own REQUEST_TIME from here.
fn serve(
    stream: &mut TcpStream,
    received: &[u8],
    host: &str,
    token: &str,
    directory: &Path,
    valid: impl Fn() -> bool,
) -> Result<bool, String> {
    stream.set_nonblocking(false).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + REQUEST_TIME;
    let mut reader = Cursor::new(received).chain(BufReader::new(&mut *stream));
    let request = match request(&mut reader, &valid, deadline) {
        Ok(request) => request,
        Err(error) => {
            error_response(stream, "400 Bad Request", &error)?;
            return Ok(false);
        }
    };
    if request.foreign(host) {
        error_response(stream, "403 Forbidden", "Open the address shown on your TV")?;
        return Ok(false);
    }
    if !valid() {
        return Err("Upload session ended".into());
    }
    if request.method == "GET" && request.length == 0 && request.target == format!("/{token}") {
        redirect_to_page(stream, token)?;
        return Ok(false);
    }
    let Some(route) = request.target.strip_prefix(&format!("/{token}/")) else {
        error_response(stream, "404 Not Found", "Upload session not found")?;
        return Ok(false);
    };
    if request.method == "GET" && request.length == 0 {
        if route == "info.json" {
            let body = serde_json::json!({"path": directory.to_string_lossy()}).to_string();
            response(
                stream,
                "200 OK",
                "application/json; charset=utf-8",
                body.as_bytes(),
            )?;
            return Ok(false);
        }
        let (kind, body): (&str, &[u8]) = match route {
            "" => (
                "text/html; charset=utf-8",
                include_bytes!("../assets/wallpaper-upload/index.html"),
            ),
            "app.css" => (
                "text/css; charset=utf-8",
                include_bytes!("../assets/wallpaper-upload/app.css"),
            ),
            "app.js" => (
                "text/javascript; charset=utf-8",
                include_bytes!("../assets/wallpaper-upload/app.js"),
            ),
            "crop.js" => (
                "text/javascript; charset=utf-8",
                include_bytes!("../assets/wallpaper-upload/crop.js"),
            ),
            "storage.js" => (
                "text/javascript; charset=utf-8",
                include_bytes!("../assets/wallpaper-upload/storage.js"),
            ),
            "astro-mark.svg" => ("image/svg+xml", include_bytes!("../assets/app-icon.svg")),
            _ => {
                error_response(stream, "404 Not Found", "Not found")?;
                return Ok(false);
            }
        };
        response(stream, "200 OK", kind, body)?;
        return Ok(false);
    }
    if request.method != "POST"
        || route != "upload"
        || request.headers.get("x-astro-upload").map(String::as_str) != Some(token)
    {
        error_response(stream, "403 Forbidden", "Invalid upload request")?;
        return Ok(false);
    }
    if !request
        .headers
        .get("content-type")
        .is_some_and(|value| matches!(value.as_str(), "image/jpeg" | "image/png" | "image/webp"))
    {
        error_response(
            stream,
            "415 Unsupported Media Type",
            "Choose a JPEG, PNG or WebP image",
        )?;
        return Ok(false);
    }
    let mut body = vec![0; request.length];
    read_exact(&mut reader, &mut body, &valid, deadline)?;
    drop(reader);
    match store(directory, &body, valid) {
        Ok(item) => {
            // The commit succeeded even if the phone disconnects before receiving its reply.
            let _ = response(
                stream,
                "201 Created",
                "application/json; charset=utf-8",
                serde_json::json!({"id":item.id}).to_string().as_bytes(),
            );
            Ok(true)
        }
        Err(error) => {
            error_response(stream, "422 Unprocessable Content", &error)?;
            Ok(false)
        }
    }
}

#[cfg(test)]
#[path = "wallpapers-tests.rs"]
mod tests;
