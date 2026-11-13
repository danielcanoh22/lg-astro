//! Decodes raster images (wallpapers, thumbnails, Recents previews) on a worker thread.
//!
//! The SDL thread only queues paths and uploads finished pixels to the GPU; each reply wakes the
//! event loop through [`crate::wake::wake`].
use std::{
    fs,
    io::{Cursor, Read},
    path::Path,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

/// Largest accepted width or height: 4K wallpaper variants are 3840 × 2160.
const MAX_DIMENSION: u32 = 4096;
/// Largest accepted file size.
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
/// Decoder allocation cap: a 4096 × 4096 RGBA frame plus decoder working memory.
const MAX_ALLOC: u64 = 80 * 1024 * 1024;

/// Decoded 8-bit pixels, tightly packed, ready for a texture upload.
pub enum Pixels {
    /// Opaque images keep three channels: a quarter less memory and no RGBA expansion.
    Rgb(image::RgbImage),
    /// Images with an alpha channel, not premultiplied.
    Rgba(image::RgbaImage),
}

impl Pixels {
    pub fn width(&self) -> u32 {
        match self {
            Self::Rgb(image) => image.width(),
            Self::Rgba(image) => image.width(),
        }
    }

    pub fn height(&self) -> u32 {
        match self {
            Self::Rgb(image) => image.height(),
            Self::Rgba(image) => image.height(),
        }
    }

    pub fn has_alpha(&self) -> bool {
        matches!(self, Self::Rgba(_))
    }

    /// Returns the packed rows, top row first.
    pub fn raw(&self) -> &[u8] {
        match self {
            Self::Rgb(image) => image.as_raw(),
            Self::Rgba(image) => image.as_raw(),
        }
    }
}

/// A worker reply: the requested path and its pixels, or `None` when decoding failed.
pub struct Decoded {
    pub path: String,
    pub pixels: Option<Pixels>,
}

/// Reads an image file without following a final symlink, bounded in file size, dimensions
/// and decoder allocations.
///
/// # Errors
///
/// Fails when the path is not a regular file, exceeds [`MAX_FILE_BYTES`], [`MAX_DIMENSION`] or
/// [`MAX_ALLOC`], or cannot be read or decoded.
pub fn decode(path: &Path) -> Result<Pixels, String> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    // Non-blocking so a FIFO in place of an image cannot stall the worker; regular files
    // ignore the flag.
    #[cfg(all(feature = "webos", unix))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(not(all(feature = "webos", unix)))]
    if !fs::symlink_metadata(path)
        .map_err(|e| e.to_string())?
        .is_file()
    {
        return Err("Image is not a regular file".into());
    }
    let file = options.open(path).map_err(|e| e.to_string())?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() {
        return Err("Image is not a regular file".into());
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err("Image exceeds 32 MiB".into());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err("Image exceeds 32 MiB".into());
    }
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_ALLOC);
    reader.limits(limits);
    let image = reader.decode().map_err(|e| e.to_string())?;
    Ok(if image.color().has_alpha() {
        Pixels::Rgba(image.into_rgba8())
    } else {
        Pixels::Rgb(image.into_rgb8())
    })
}

/// The handle to the `astro-images` decoding worker, which decodes requests in order.
pub struct Loader {
    requests: Option<mpsc::Sender<String>>,
    replies: mpsc::Receiver<Decoded>,
}

impl Loader {
    /// Starts the worker with the bounded file [`decode`].
    pub fn new() -> Self {
        Self::with_decoder(|path| decode(Path::new(path)))
    }

    /// Starts the worker with a custom decoder; tests inject failing or slow ones.
    fn with_decoder(decoder: impl Fn(&str) -> Result<Pixels, String> + Send + 'static) -> Self {
        let (requests, queue) = mpsc::channel::<String>();
        let (sender, replies) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("astro-images".into())
            .spawn(move || {
                for path in queue {
                    // A decoder panic fails this image instead of ending the worker, which would
                    // leave every queued path pending forever.
                    let result =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| decoder(&path)))
                            .unwrap_or_else(|_| Err("decoder panicked".into()));
                    let pixels = result
                        .map_err(|error| eprintln!("Artwork {path}: {error}"))
                        .ok();
                    if sender.send(Decoded { path, pixels }).is_err() {
                        return;
                    }
                    crate::wake::wake();
                }
            });
        if let Err(error) = &spawned {
            eprintln!("Astro image worker: {error}");
        }
        Self {
            requests: spawned.is_ok().then_some(requests),
            replies,
        }
    }

    /// Queues a decode, returning `false` when the worker is unavailable so the caller can
    /// decode on its own thread instead of waiting forever.
    pub fn request(&self, path: &str) -> bool {
        self.requests
            .as_ref()
            .is_some_and(|requests| requests.send(path.into()).is_ok())
    }

    /// Returns a finished decode without blocking.
    pub fn try_recv(&self) -> Option<Decoded> {
        self.replies.try_recv().ok()
    }

    /// Waits for the next decoded image until `deadline`.
    pub fn recv_until(&self, deadline: Instant) -> Option<Decoded> {
        let timeout = deadline.saturating_duration_since(Instant::now());
        if timeout == Duration::ZERO {
            return None;
        }
        self.replies.recv_timeout(timeout).ok()
    }
}

#[cfg(test)]
#[path = "image-loader-tests.rs"]
mod tests;
