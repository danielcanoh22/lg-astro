use super::*;
use std::path::PathBuf;

struct Scratch(PathBuf);
impl Scratch {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("astro-image-loader-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write(path: &Path, image: image::DynamicImage, format: image::ImageFormat) {
    image.save_with_format(path, format).unwrap();
}

#[test]
fn opaque_images_decode_to_three_channels() {
    let scratch = Scratch::new("opaque");
    let path = scratch.0.join("wallpaper.jpg");
    write(
        &path,
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            64,
            36,
            image::Rgb([38, 56, 50]),
        )),
        image::ImageFormat::Jpeg,
    );
    let pixels = decode(&path).unwrap();
    assert!(!pixels.has_alpha());
    assert_eq!((pixels.width(), pixels.height()), (64, 36));
    assert_eq!(pixels.raw().len(), 64 * 36 * 3);
}

#[test]
fn transparent_images_keep_their_alpha() {
    let scratch = Scratch::new("alpha");
    let path = scratch.0.join("icon.png");
    write(
        &path,
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            8,
            4,
            image::Rgba([255, 0, 0, 128]),
        )),
        image::ImageFormat::Png,
    );
    let pixels = decode(&path).unwrap();
    assert!(pixels.has_alpha());
    assert_eq!(pixels.raw().len(), 8 * 4 * 4);
    assert_eq!(&pixels.raw()[..4], &[255, 0, 0, 128]);
}

#[test]
fn dimensions_beyond_the_limit_are_rejected_before_decoding() {
    let scratch = Scratch::new("large");
    let path = scratch.0.join("wide.png");
    write(
        &path,
        image::DynamicImage::ImageRgb8(image::RgbImage::new(MAX_DIMENSION + 1, 1)),
        image::ImageFormat::Png,
    );
    assert!(decode(&path).is_err());
}

#[test]
fn directories_and_missing_files_are_not_images() {
    let scratch = Scratch::new("kinds");
    assert!(decode(&scratch.0).is_err());
    assert!(decode(&scratch.0.join("missing.jpg")).is_err());
    let garbage = scratch.0.join("garbage.jpg");
    fs::write(&garbage, b"not an image").unwrap();
    assert!(decode(&garbage).is_err());
}

#[cfg(unix)]
#[test]
fn a_symlinked_image_is_not_followed() {
    let scratch = Scratch::new("symlink");
    let target = scratch.0.join("target.png");
    write(
        &target,
        image::DynamicImage::ImageRgb8(image::RgbImage::new(4, 4)),
        image::ImageFormat::Png,
    );
    let link = scratch.0.join("link.png");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(decode(&target).is_ok());
    assert!(decode(&link).is_err());
}

#[test]
fn the_worker_answers_every_request_in_order_with_failures_as_none() {
    let loader = Loader::with_decoder(|path| {
        if path == "broken" {
            Err("broken".into())
        } else {
            Ok(Pixels::Rgb(image::RgbImage::new(2, 1)))
        }
    });
    assert!(loader.request("first"));
    assert!(loader.request("broken"));
    let deadline = Instant::now() + Duration::from_secs(5);
    let first = loader.recv_until(deadline).expect("first reply");
    assert_eq!(first.path, "first");
    assert_eq!(first.pixels.map(|p| p.raw().len()), Some(6));
    let broken = loader.recv_until(deadline).expect("second reply");
    assert_eq!(broken.path, "broken");
    assert!(broken.pixels.is_none());
    assert!(loader.try_recv().is_none());
}

#[test]
fn a_decoder_panic_fails_that_image_and_the_worker_keeps_going() {
    let loader = Loader::with_decoder(|path| {
        if path == "panics" {
            panic!("corrupt image");
        }
        Ok(Pixels::Rgb(image::RgbImage::new(1, 1)))
    });
    assert!(loader.request("panics"));
    assert!(loader.request("after"));
    let deadline = Instant::now() + Duration::from_secs(5);
    let failed = loader.recv_until(deadline).expect("panicked reply");
    assert_eq!(failed.path, "panics");
    assert!(failed.pixels.is_none());
    let next = loader.recv_until(deadline).expect("later reply");
    assert_eq!(next.path, "after");
    assert!(next.pixels.is_some());
}

#[test]
fn waiting_past_the_deadline_returns_nothing() {
    let loader = Loader::with_decoder(|_| {
        std::thread::sleep(Duration::from_millis(200));
        Ok(Pixels::Rgb(image::RgbImage::new(1, 1)))
    });
    assert!(loader.request("slow"));
    assert!(
        loader
            .recv_until(Instant::now() + Duration::from_millis(10))
            .is_none()
    );
    assert!(loader.recv_until(Instant::now()).is_none());
}
