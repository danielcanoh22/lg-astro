use super::*;
use crate::companion::{HEADER_TIME, PENDING_LIMIT, PENDING_PEER_LIMIT, Pending, TOKEN_ALPHABET};
use std::{
    net::{Ipv4Addr, TcpListener},
    sync::atomic::AtomicUsize,
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "astro-wallpapers-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn wallpapers(&self) -> PathBuf {
        self.0.join("wallpapers")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn image(width: u32, height: u32) -> Vec<u8> {
    jpeg(
        &image::RgbImage::from_pixel(width, height, image::Rgb([150, 200, 230])),
        85,
    )
    .unwrap()
}

/// A minimal catalog entry with valid metadata and non-empty image files.
fn listed_wallpaper(root: &Path, index: usize) -> String {
    let id = format!("custom-{index:032x}");
    let directory = root.join(&id);
    create_directory(&directory).unwrap();
    write_new(
        &directory.join("metadata.json"),
        &serde_json::to_vec(&Wallpaper {
            id: id.clone(),
            directory: directory.clone(),
            has_original: false,
        })
        .unwrap(),
    )
    .unwrap();
    write_new(&directory.join("wallpaper.jpg"), b"fixture").unwrap();
    write_new(&directory.join("thumbnail.jpg"), b"fixture").unwrap();
    id
}

/// The first catalog the worker reports, after its startup cleanup.
fn first_catalog(fixture: &Fixture) -> Vec<Wallpaper> {
    let service = Service::new(&fixture.0.join("preferences.json"), true).unwrap();
    match service
        .receiver
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
    {
        Event::Catalog(items) => items,
        _ => panic!("Expected the initial catalog"),
    }
}

/// Backdates an entry past the age at which startup treats staging as abandoned.
fn abandon(path: &Path) {
    fs::File::open(path)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - STAGING_ABANDONED - Duration::from_secs(60))
        .unwrap();
}

/// The HTTP tests use only a loopback mock, never the TV or any external service.
fn http(path: &Path, request: impl FnOnce(&str) -> Vec<u8>) -> (String, bool) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let path = path.to_owned();
    let worker = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        serve(
            &mut socket,
            &[],
            &address.to_string(),
            "secret",
            &path,
            || true,
        )
        .unwrap_or(false)
    });
    let mut socket = TcpStream::connect(address).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    socket.write_all(&request(&address.to_string())).unwrap();
    let mut response = String::new();
    let _ = socket.read_to_string(&mut response);
    (response, worker.join().unwrap())
}

#[test]
fn generated_ids_are_safe_and_selection_survives_preferences_reload() {
    let id = format!("custom-{}", random_id().unwrap());
    assert!(valid_id(&id));
    assert_ne!(id, format!("custom-{}", random_id().unwrap()));
    let mut preferences = crate::model::Preferences {
        background: id.clone(),
        ..Default::default()
    };
    preferences.sanitize();
    assert_eq!(preferences.background, id);
    for invalid in [
        "custom-../etc",
        "custom-",
        "custom-123",
        "custom-FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF",
        "/etc/passwd",
    ] {
        assert!(!valid_id(invalid));
        preferences.background = invalid.into();
        preferences.sanitize();
        assert_eq!(preferences.background, "dunes");
    }
}

#[test]
fn upload_tokens_are_eight_unbiased_alphanumeric_characters() {
    let tokens: Vec<_> = (0..256).map(|_| random_token().unwrap()).collect();
    for token in &tokens {
        assert_eq!(token.len(), 8);
        assert!(token.bytes().all(|byte| TOKEN_ALPHABET.contains(&byte)));
    }
    let distinct: std::collections::HashSet<_> = tokens.iter().collect();
    assert_eq!(distinct.len(), tokens.len());
}

#[test]
fn uploads_retain_full_resolution_unshaded_images_and_discover_legacy_fallbacks() {
    let fixture = Fixture::new();
    let item = store(&fixture.wallpapers(), &image(1920, 1080), || true).unwrap();
    assert!(item.has_original);
    let original = image::open(item.unshaded_image()).unwrap().to_rgb8();
    let shaded = image::open(item.image()).unwrap().to_rgb8();
    assert_eq!(original.dimensions(), (1920, 1080));
    assert_ne!(original.get_pixel(960, 1000), shaded.get_pixel(960, 1000));
    assert!(catalog(&fixture.wallpapers())[0].has_original);
    fs::remove_file(item.directory.join("original.jpg")).unwrap();
    let legacy = catalog(&fixture.wallpapers());
    assert_eq!(legacy.len(), 1);
    assert!(!legacy[0].has_original);
    assert_eq!(legacy[0].unshaded_image(), legacy[0].image());
}

#[test]
fn deletion_protects_selected_and_bundled_backgrounds_and_only_removes_the_requested_upload() {
    let fixture = Fixture::new();
    let bytes = image(1920, 1080);
    let first = store(&fixture.wallpapers(), &bytes, || true).unwrap();
    let second = store(&fixture.wallpapers(), &bytes, || true).unwrap();
    assert!(delete_wallpaper(&fixture.wallpapers(), &first.id, &first.id).is_err());
    for id in ["dunes", "../preferences.json", "custom-../../other"] {
        assert!(delete_wallpaper(&fixture.wallpapers(), id, "dunes").is_err());
    }
    assert_eq!(catalog(&fixture.wallpapers()).len(), 2);
    delete_wallpaper(&fixture.wallpapers(), &first.id, &second.id).unwrap();
    assert!(!first.directory.exists());
    assert!(second.directory.join("wallpaper.jpg").exists());
    assert_eq!(catalog(&fixture.wallpapers()).len(), 1);
    assert!(delete_wallpaper(&fixture.wallpapers(), &first.id, &second.id).is_err());
}

#[cfg(unix)]
#[test]
fn deletion_rejects_symlink_roots_and_entries_and_never_follows_internal_symlinks() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let root = fixture.wallpapers();
    fs::create_dir(&root).unwrap();
    let outside = fixture.0.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("keep"), b"keep").unwrap();
    let id = format!("custom-{}", "1".repeat(32));
    symlink(&outside, root.join(&id)).unwrap();
    assert!(delete_wallpaper(&root, &id, "dunes").is_err());
    fs::remove_file(root.join(&id)).unwrap();
    fs::create_dir(root.join(&id)).unwrap();
    symlink(&outside, root.join(&id).join("linked")).unwrap();
    let linked_root = fixture.0.join("linked-root");
    symlink(&root, &linked_root).unwrap();
    assert!(delete_wallpaper(&linked_root, &id, "dunes").is_err());
    delete_wallpaper(&root, &id, "dunes").unwrap();
    assert_eq!(fs::read(outside.join("keep")).unwrap(), b"keep");
}

#[test]
fn worker_reports_deletion_before_the_updated_catalog() {
    let fixture = Fixture::new();
    let item = store(&fixture.wallpapers(), &image(1920, 1080), || true).unwrap();
    let service = Service::new(&fixture.0.join("preferences.json"), true).unwrap();
    assert!(
        matches!(service.receiver.recv_timeout(Duration::from_secs(5)).unwrap(), Event::Catalog(items) if items.len() == 1)
    );
    service.delete(item.id.clone(), "dunes".into());
    assert!(
        matches!(service.receiver.recv_timeout(Duration::from_secs(5)).unwrap(), Event::Deleted(id, Ok(())) if id == item.id)
    );
    assert!(
        matches!(service.receiver.recv_timeout(Duration::from_secs(5)).unwrap(), Event::Catalog(items) if items.is_empty())
    );
    assert!(!item.directory.exists());
}

#[test]
fn uploads_are_unnamed_normalized_immutable_and_discoverable_after_restart() {
    let fixture = Fixture::new();
    let bytes = image(1920, 1080);
    let first = store(&fixture.wallpapers(), &bytes, || true).unwrap();
    let second = store(&fixture.wallpapers(), &bytes, || true).unwrap();
    assert_ne!(first.id, second.id);
    let found = catalog(&fixture.wallpapers());
    assert_eq!(found.len(), 2);
    assert!(found.iter().any(|item| item.id == first.id));
    let background = image::open(first.image()).unwrap();
    let thumbnail = image::open(first.thumbnail()).unwrap();
    assert_eq!((background.width(), background.height()), (1920, 1080));
    assert_eq!((thumbnail.width(), thumbnail.height()), (480, 270));
    assert!(
        background.to_rgb8().get_pixel(960, 1000)[0] < thumbnail.to_rgb8().get_pixel(240, 250)[0]
    );
    assert_eq!(
        fs::read(first.directory.join("metadata.json"))
            .unwrap()
            .len(),
        serde_json::to_vec(&first).unwrap().len()
    );
}

#[test]
fn validation_and_cancellation_leave_no_partial_gallery_entries() {
    let fixture = Fixture::new();
    let root = fixture.wallpapers();
    assert!(store(&root, b"<svg></svg>", || true).is_err());
    assert!(store(&root, &image(320, 240), || true).is_err());
    assert!(store(&root, &image(1920, 1080), || false).is_err());
    assert!(catalog(&root).is_empty());
    assert!(!root.exists());
    let mut reader = Cursor::new(b"unfinished");
    assert!(
        read_exact(
            &mut reader,
            &mut [0; 20],
            &|| false,
            Instant::now() + REQUEST_TIME
        )
        .is_err()
    );
    assert!(read_exact(&mut reader, &mut [0; 20], &|| true, Instant::now()).is_err());
}

#[test]
fn cancellation_before_commit_cleans_only_the_new_staging_files() {
    let fixture = Fixture::new();
    let checks = AtomicUsize::new(0);
    let result = store(&fixture.wallpapers(), &image(1920, 1080), || {
        checks.fetch_add(1, Ordering::Relaxed) == 0
    });
    assert!(result.is_err());
    assert_eq!(checks.load(Ordering::Relaxed), 2);
    assert!(fs::read_dir(fixture.wallpapers()).unwrap().next().is_none());
}

#[test]
fn gallery_capacity_is_checked_before_decoding_or_creating_another_upload() {
    let fixture = Fixture::new();
    create_directory(&fixture.wallpapers()).unwrap();
    for index in 0..MAX_WALLPAPERS {
        listed_wallpaper(&fixture.wallpapers(), index);
    }
    assert_eq!(catalog(&fixture.wallpapers()).len(), MAX_WALLPAPERS);
    assert!(
        store(&fixture.wallpapers(), b"not decoded", || true)
            .unwrap_err()
            .contains("full")
    );
    assert_eq!(
        fs::read_dir(fixture.wallpapers()).unwrap().count(),
        MAX_WALLPAPERS
    );
}

#[test]
fn unrelated_directory_entries_do_not_hide_wallpapers_from_the_catalog() {
    let fixture = Fixture::new();
    let root = fixture.wallpapers();
    create_directory(&root).unwrap();
    let mut ids: Vec<_> = (0..5).map(|index| listed_wallpaper(&root, index)).collect();
    ids.sort();
    // Far more entries than the 128-entry scan bound, created after the wallpapers (tmpfs
    // lists the newest first), so a bound on every entry would miss wallpapers in practice.
    for index in 0..1000 {
        let name = match index % 3 {
            0 => format!("junk-{index}"),
            1 => format!(".hidden-{index}"),
            _ => format!("custom-{index}"),
        };
        fs::write(root.join(name), b"junk").unwrap();
    }
    let found: Vec<_> = catalog(&root).into_iter().map(|item| item.id).collect();
    assert_eq!(found, ids);
}

#[test]
fn worker_start_removes_interrupted_upload_staging_and_nothing_else() {
    let fixture = Fixture::new();
    let item = store(&fixture.wallpapers(), &image(1920, 1080), || true).unwrap();
    let root = fixture.wallpapers();
    let staging = root.join(format!(".custom-{}", "a".repeat(32)));
    create_directory(&staging).unwrap();
    write_new(&staging.join("wallpaper.jpg"), b"partial").unwrap();
    write_new(&staging.join("original.jpg"), b"partial").unwrap();
    abandon(&staging);
    let kept = [
        ".keep".to_string(),
        ".custom-notvalid".into(),
        format!(".custom-{}", "A".repeat(32)),
        format!("..custom-{}", "b".repeat(32)),
    ];
    for name in &kept {
        fs::create_dir(root.join(name)).unwrap();
        fs::write(root.join(name).join("keep"), b"keep").unwrap();
        abandon(&root.join(name));
    }
    let items = first_catalog(&fixture);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].id, item.id);
    assert!(fs::symlink_metadata(&staging).is_err());
    assert!(item.directory.join("wallpaper.jpg").exists());
    for name in &kept {
        assert_eq!(fs::read(root.join(name).join("keep")).unwrap(), b"keep");
    }
}

#[cfg(unix)]
#[test]
fn worker_start_never_follows_staging_symlinks() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let root = fixture.wallpapers();
    create_directory(&root).unwrap();
    let outside = fixture.0.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("keep"), b"keep").unwrap();
    let link = root.join(format!(".custom-{}", "c".repeat(32)));
    symlink(&outside, &link).unwrap();
    let staging = root.join(format!(".custom-{}", "d".repeat(32)));
    create_directory(&staging).unwrap();
    symlink(&outside, staging.join("linked")).unwrap();
    abandon(&staging);
    assert!(first_catalog(&fixture).is_empty());
    // std cannot backdate a symlink's own mtime, so this link is fresh and must be kept; an
    // abandoned one is unlinked with remove_file, which never touches its target.
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(fs::symlink_metadata(&staging).is_err());
    assert_eq!(fs::read(outside.join("keep")).unwrap(), b"keep");
}

#[test]
fn worker_start_keeps_fresh_staging_another_process_may_still_be_writing() {
    let fixture = Fixture::new();
    let root = fixture.wallpapers();
    create_directory(&root).unwrap();
    let staging = root.join(format!(".custom-{}", "f".repeat(32)));
    create_directory(&staging).unwrap();
    write_new(&staging.join("wallpaper.jpg"), b"partial").unwrap();
    assert!(first_catalog(&fixture).is_empty());
    assert_eq!(fs::read(staging.join("wallpaper.jpg")).unwrap(), b"partial");
}

#[cfg(unix)]
#[test]
fn worker_start_leaves_a_symlinked_wallpaper_directory_alone() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let elsewhere = fixture.0.join("elsewhere");
    let staging = elsewhere.join(format!(".custom-{}", "e".repeat(32)));
    fs::create_dir_all(&staging).unwrap();
    fs::write(staging.join("keep"), b"keep").unwrap();
    abandon(&staging);
    symlink(&elsewhere, fixture.wallpapers()).unwrap();
    assert!(first_catalog(&fixture).is_empty());
    assert_eq!(fs::read(staging.join("keep")).unwrap(), b"keep");
}

#[test]
fn decoder_enforces_transport_dimensions_and_composites_transparent_pngs() {
    assert!(decode(&vec![0; MAX_BODY + 1]).is_err());
    let rgba = image::RgbaImage::from_pixel(1920, 1080, image::Rgba([255, 0, 0, 0]));
    let mut png = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(rgba)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    assert_eq!(
        decode(png.get_ref()).unwrap().get_pixel(0, 0).0,
        [23, 25, 28]
    );
    let too_tall = image::RgbaImage::new(1, 12000);
    let mut png = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(too_tall)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    assert!(decode(png.get_ref()).is_err());
}

/// A 1920 × 1080 image whose every pixel differs from its neighbours.
fn pattern() -> image::RgbImage {
    image::RgbImage::from_fn(1920, 1080, |x, y| {
        image::Rgb([
            (x * 7 + y * 3) as u8,
            ((x * 13) ^ (y * 5)) as u8,
            (x + y * 11) as u8,
        ])
    })
}

fn png(image: image::DynamicImage) -> Vec<u8> {
    let mut png = Cursor::new(Vec::new());
    image.write_to(&mut png, image::ImageFormat::Png).unwrap();
    png.into_inner()
}

#[test]
fn opaque_uploads_keep_their_exact_pixels_and_translucent_ones_composite_on_the_base() {
    let opaque = pattern();
    assert_eq!(
        decode(&png(image::DynamicImage::ImageRgb8(opaque.clone()))).unwrap(),
        opaque
    );
    let translucent = image::RgbaImage::from_pixel(1920, 1080, image::Rgba([255, 0, 100, 128]));
    // 255·128/255 + 23·127/255 = 139.45, 0 + 25·127/255 = 12.45, 100·128/255 + 28·127/255 = 64.14
    assert_eq!(
        decode(&png(image::DynamicImage::ImageRgba8(translucent)))
            .unwrap()
            .get_pixel(5, 5)
            .0,
        [139, 12, 64]
    );
}

#[test]
fn shading_matches_the_bundled_wallpaper_gradient_math() {
    // The per-pixel formula shade() must reproduce exactly (tools/assets.mjs gradients).
    fn reference(image: &mut image::RgbImage) {
        let stops = [
            (0., [13., 16., 19.], 0.24),
            (0.19, [14., 17., 21.], 0.08),
            (0.35, [15., 17., 20.], 0.35),
            (0.63, [17., 19., 23.], 0.86),
            (0.85, [23., 25., 29.], 0.92157),
            (1., [23., 25., 29.], 0.96078),
        ];
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            let y = y as f32 / 1079.;
            let pair = stops.windows(2).find(|s| y <= s[1].0).unwrap();
            let t = (y - pair[0].0) / (pair[1].0 - pair[0].0);
            let alpha: f32 = pair[0].2 * (1. - t) + pair[1].2 * t;
            let horizontal = 0.42 * (1. - x as f32 / (1919. * 0.75)).max(0.);
            for i in 0..3 {
                let base = pixel[i] as f32 * 0.92 + [23., 25., 28.][i] * 0.08;
                let base = base * (1. - horizontal) + [17., 20., 23.][i] * horizontal;
                let gradient = pair[0].1[i] * pair[0].2 * (1. - t) + pair[1].1[i] * pair[1].2 * t;
                pixel[i] = (base * (1. - alpha) + gradient).round() as u8;
            }
        }
    }
    let mut expected = pattern();
    reference(&mut expected);
    let mut shaded = pattern();
    shade(&mut shaded);
    assert!(shaded == expected, "shade() changed pixels");
}

#[test]
fn thumbnails_are_the_exact_average_of_each_4x4_block() {
    let source = pattern();
    let small = thumbnail(&source);
    assert_eq!(small.dimensions(), (480, 270));
    for (x, y) in [(0, 0), (1, 0), (239, 134), (479, 269), (100, 200)] {
        let mut sum = [0u32; 3];
        for dy in 0..4 {
            for dx in 0..4 {
                let p = source.get_pixel(x * 4 + dx, y * 4 + dy).0;
                for i in 0..3 {
                    sum[i] += p[i] as u32;
                }
            }
        }
        let average = sum.map(|channel| ((channel + 8) / 16) as u8);
        assert_eq!(small.get_pixel(x, y).0, average, "block ({x}, {y})");
    }
}

#[test]
fn protocol_rejects_oversized_ambiguous_and_chunked_requests() {
    for request_text in [
        format!(
            "POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY + 1
        ),
        "POST / HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n".into(),
        "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n".into(),
        "POST / HTTP/1.1\r\nContent-Length: -1\r\n\r\n".into(),
        "POST / HTTP/1.1\r\nContent-Length: +1\r\n\r\n".into(),
        "POST / HTTP/1.1\r\nContent-Length: 9999999999999999999999999\r\n\r\n".into(),
        format!("GET / HTTP/1.1\r\nX-Header: {}", "a".repeat(8192)),
    ] {
        assert!(
            request(
                &mut Cursor::new(request_text),
                &|| true,
                Instant::now() + REQUEST_TIME
            )
            .is_err()
        );
    }
}

#[test]
fn only_the_session_origin_host_token_and_upload_header_allow_writes() {
    let fixture = Fixture::new();
    for (route, headers, expected) in [
        ("/wrong/", "", "404 Not Found"),
        ("/secret/../../etc/passwd", "", "404 Not Found"),
        (
            "/secret/",
            "Origin: https://evil.example\r\n",
            "403 Forbidden",
        ),
        (
            "/secret/",
            "Sec-Fetch-Site: cross-site\r\n",
            "403 Forbidden",
        ),
    ] {
        let (response, changed) = http(&fixture.wallpapers(), |host| {
            format!("GET {route} HTTP/1.1\r\nHost: {host}\r\n{headers}\r\n").into_bytes()
        });
        assert!(
            response.starts_with(&format!("HTTP/1.1 {expected}")),
            "{response}"
        );
        assert!(!changed);
    }
    let (response, changed) = http(&fixture.wallpapers(), |_| {
        b"GET /secret/ HTTP/1.1\r\nHost: attacker.example\r\n\r\n".to_vec()
    });
    assert!(response.starts_with("HTTP/1.1 403"));
    assert!(!changed);
    let (response, changed) = http(&fixture.wallpapers(), |host| {
        format!("POST /secret/upload HTTP/1.1\r\nHost: {host}\r\nContent-Length: 0\r\nContent-Type: image/jpeg\r\n\r\n").into_bytes()
    });
    assert!(response.starts_with("HTTP/1.1 403"));
    assert!(!changed);
    assert!(!fixture.wallpapers().exists());
}

#[test]
fn slashless_session_urls_redirect_to_the_local_asset_base() {
    let fixture = Fixture::new();
    let (response, changed) = http(&fixture.wallpapers(), |host| {
        format!("GET /secret HTTP/1.1\r\nHost: {host}\r\n\r\n").into_bytes()
    });
    assert!(response.starts_with("HTTP/1.1 303 See Other\r\n"));
    assert!(response.contains("\r\nLocation: /secret/\r\n"));
    assert!(response.contains("Cache-Control: no-store"));
    assert!(!changed);
    for (target, headers, expected) in [
        ("/wrong", "", "404 Not Found"),
        (
            "/secret",
            "Origin: https://evil.example\r\n",
            "403 Forbidden",
        ),
    ] {
        let (response, changed) = http(&fixture.wallpapers(), |host| {
            format!("GET {target} HTTP/1.1\r\nHost: {host}\r\n{headers}\r\n").into_bytes()
        });
        assert!(response.starts_with(&format!("HTTP/1.1 {expected}")));
        assert!(!response.contains("\r\nLocation:"));
        assert!(!changed);
    }
}

#[test]
fn browser_assets_are_local_and_a_real_http_upload_needs_no_name() {
    let fixture = Fixture::new();
    for route in [
        "",
        "app.js",
        "crop.js",
        "storage.js",
        "app.css",
        "astro-mark.svg",
        "info.json",
    ] {
        let (response, changed) = http(&fixture.wallpapers(), |host| {
            format!("GET /secret/{route} HTTP/1.1\r\nHost: {host}\r\n\r\n").into_bytes()
        });
        assert!(response.starts_with("HTTP/1.1 200"));
        assert!(response.contains("Cache-Control: no-store"));
        assert!(response.contains("frame-ancestors 'none'"));
        assert!(!changed);
        if route == "astro-mark.svg" {
            assert!(response.starts_with("HTTP/1.1 200"));
            assert!(response.contains("image/svg+xml"));
            assert!(response.contains("<svg"));
        }
        if route == "info.json" {
            assert!(response.starts_with("HTTP/1.1 200"));
            assert!(response.contains(&fixture.wallpapers().to_string_lossy().to_string()));
        }
    }
    let bytes = image(1920, 1080);
    let (response, changed) = http(&fixture.wallpapers(), |host| {
        let mut wire = format!("POST /secret/upload HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nX-Astro-Upload: secret\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n", bytes.len()).into_bytes();
        wire.extend_from_slice(&bytes);
        wire
    });
    assert!(response.starts_with("HTTP/1.1 201"), "{response}");
    assert!(changed);
    let items = catalog(&fixture.wallpapers());
    assert_eq!(items.len(), 1);
    assert!(response.contains(&items[0].id));
    let metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(items[0].directory.join("metadata.json")).unwrap())
            .unwrap();
    assert_eq!(metadata, serde_json::json!({"id": items[0].id}));
}

#[test]
fn legacy_named_uploads_keep_their_ids_and_load_without_names() {
    let fixture = Fixture::new();
    let item = store(&fixture.wallpapers(), &image(1920, 1080), || true).unwrap();
    assert!(item.directory.starts_with(fixture.wallpapers()));
    assert!(valid_id(&item.id));
    fs::write(
        item.directory.join("metadata.json"),
        serde_json::to_vec(&serde_json::json!({"id": item.id, "name": "旅行 & sea"})).unwrap(),
    )
    .unwrap();
    let items = catalog(&fixture.wallpapers());
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].id, item.id);
    assert_eq!(items[0].image(), item.image());
}

#[cfg(unix)]
#[test]
fn symlink_storage_and_gallery_entries_are_never_followed() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let elsewhere = fixture.0.join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    symlink(&elsewhere, fixture.wallpapers()).unwrap();
    assert!(store(&fixture.wallpapers(), &image(1920, 1080), || true).is_err());
    assert!(fs::read_dir(&elsewhere).unwrap().next().is_none());
    assert!(catalog(&fixture.wallpapers()).is_empty());
}

#[test]
fn qr_generation_is_deterministic_and_has_a_reasonable_size() {
    let address = "192.168.1.2:12345".parse().unwrap();
    let token = random_token().unwrap();
    let info = session(address, &token).unwrap();
    assert_eq!(info.url, format!("http://192.168.1.2:12345/{token}/"));
    assert_eq!(info.qr.len(), (info.qr_size * info.qr_size) as usize);
    assert!(info.qr_size <= 65);
    assert_eq!(info.qr, session(address, &token).unwrap().qr);
}

#[test]
fn session_closure_revokes_the_socket_and_reopening_rotates_the_capability() {
    let fixture = Fixture::new();
    let mut service = Service::new(&fixture.0.join("preferences.json"), true).unwrap();
    assert!(matches!(
        service
            .receiver
            .recv_timeout(Duration::from_secs(3))
            .unwrap(),
        Event::Catalog(_)
    ));
    service.set_active(true);
    let first = match service
        .receiver
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
    {
        Event::Session(generation, Ok(session)) => {
            assert_eq!(generation, service.generation());
            session
        }
        _ => panic!("Expected a local upload session"),
    };
    let address = first
        .url
        .strip_prefix("http://")
        .unwrap()
        .split('/')
        .next()
        .unwrap();
    assert!(address.starts_with("127.0.0.1:"));
    service.set_active(false);
    assert!(matches!(
        service
            .receiver
            .recv_timeout(Duration::from_secs(3))
            .unwrap(),
        Event::Ended(_)
    ));
    assert!(TcpStream::connect(address).is_err());
    service.set_active(true);
    let second = match service
        .receiver
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
    {
        Event::Session(_, Ok(session)) => session,
        _ => panic!("Expected a new upload session"),
    };
    assert_ne!(first.url, second.url);
    assert_ne!(first.url.split('/').nth(3), second.url.split('/').nth(3));
    service.set_active(false);
    assert!(matches!(
        service
            .receiver
            .recv_timeout(Duration::from_secs(3))
            .unwrap(),
        Event::Ended(_)
    ));
}

#[test]
fn cancelling_a_slow_request_releases_the_worker_without_waiting_for_the_body() {
    let fixture = Fixture::new();
    let mut service = Service::new(&fixture.0.join("preferences.json"), true).unwrap();
    service
        .receiver
        .recv_timeout(Duration::from_secs(3))
        .unwrap();
    service.set_active(true);
    let info = match service
        .receiver
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
    {
        Event::Session(_, Ok(session)) => session,
        _ => panic!("Expected a session"),
    };
    let parts: Vec<_> = info.url.split('/').collect();
    let mut socket = TcpStream::connect(parts[2]).unwrap();
    write!(socket, "POST /{}/upload HTTP/1.1\r\nHost: {}\r\nX-Astro-Upload: {}\r\nContent-Type: image/jpeg\r\nContent-Length: 100000\r\n\r\npartial", parts[3], parts[2], parts[3]).unwrap();
    service.set_active(false);
    assert!(matches!(
        service
            .receiver
            .recv_timeout(Duration::from_secs(3))
            .unwrap(),
        Event::Ended(_)
    ));
    assert!(catalog(&fixture.wallpapers()).is_empty());
}

/// Opens a loopback upload session and returns the service, its `host:port` and its token.
fn open_session(fixture: &Fixture) -> (Service, String, String) {
    let mut service = Service::new(&fixture.0.join("preferences.json"), true).unwrap();
    service
        .receiver
        .recv_timeout(Duration::from_secs(3))
        .unwrap();
    service.set_active(true);
    let Event::Session(_, Ok(session)) = service
        .receiver
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
    else {
        panic!("Expected a session");
    };
    let parts: Vec<_> = session.url.split('/').collect();
    (service, parts[2].to_owned(), parts[3].to_owned())
}

/// Sends one request and returns the response, or whatever arrived within `wait`.
fn exchange(address: &str, request: &[u8], wait: Duration) -> String {
    let mut socket = TcpStream::connect(address).unwrap();
    socket.set_read_timeout(Some(wait)).unwrap();
    socket.write_all(request).unwrap();
    let mut response = Vec::new();
    let _ = socket.read_to_end(&mut response);
    String::from_utf8_lossy(&response).into_owned()
}

fn page_request(address: &str, token: &str) -> Vec<u8> {
    format!("GET /{token}/ HTTP/1.1\r\nHost: {address}\r\n\r\n").into_bytes()
}

/// A connection that sent part of its request line and then went quiet.
fn stalled(address: &str) -> TcpStream {
    let mut socket = TcpStream::connect(address).unwrap();
    socket.write_all(b"GET /").unwrap();
    socket
}

#[test]
fn requests_without_the_token_do_not_count_toward_the_session_cap() {
    let fixture = Fixture::new();
    let (_service, address, token) = open_session(&fixture);
    let tokenless = format!("GET /favicon.ico HTTP/1.1\r\nHost: {address}\r\n\r\n");
    for _ in 0..SESSION_REQUEST_LIMIT + 4 {
        let response = exchange(&address, tokenless.as_bytes(), Duration::from_secs(3));
        assert!(response.starts_with("HTTP/1.1 404 "), "{response}");
    }
    let response = exchange(
        &address,
        &page_request(&address, &token),
        Duration::from_secs(3),
    );
    assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
}

#[test]
fn a_peer_stalling_its_headers_does_not_hold_up_other_requests() {
    let fixture = Fixture::new();
    let (_service, address, token) = open_session(&fixture);
    let quiet: Vec<_> = (1..PENDING_PEER_LIMIT).map(|_| stalled(&address)).collect();
    let started = Instant::now();
    let response = exchange(
        &address,
        &page_request(&address, &token),
        Duration::from_secs(2),
    );
    assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
    assert!(started.elapsed() < Duration::from_secs(2));
    drop(quiet);
}

#[test]
fn headers_that_miss_their_deadline_are_rejected_before_the_request_time() {
    let fixture = Fixture::new();
    let (_service, address, token) = open_session(&fixture);
    let started = Instant::now();
    let response = exchange(
        &address,
        format!("GET /{token}/ HTTP/1.1\r\nHost: {address}\r\n").as_bytes(),
        HEADER_TIME + Duration::from_secs(2),
    );
    assert!(response.starts_with("HTTP/1.1 400 "), "{response}");
    assert!(response.contains("Request headers timed out"), "{response}");
    assert!(started.elapsed() >= HEADER_TIME);
    assert!(started.elapsed() < HEADER_TIME + Duration::from_secs(2));
    let response = exchange(
        &address,
        &page_request(&address, &token),
        Duration::from_secs(3),
    );
    assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
}

#[test]
fn an_upload_body_keeps_its_own_time_after_the_headers_arrive() {
    let fixture = Fixture::new();
    let (_service, address, token) = open_session(&fixture);
    let bytes = image(1920, 1080);
    let mut socket = TcpStream::connect(&address).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(socket, "POST /{token}/upload HTTP/1.1\r\nHost: {address}\r\nX-Astro-Upload: {token}\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n", bytes.len()).unwrap();
    thread::sleep(HEADER_TIME + Duration::from_secs(1));
    socket.write_all(&bytes).unwrap();
    let mut response = String::new();
    let _ = socket.read_to_string(&mut response);
    assert!(response.starts_with("HTTP/1.1 201 "), "{response}");
    assert_eq!(catalog(&fixture.wallpapers()).len(), 1);
}

#[test]
fn one_peer_cannot_hold_more_than_its_share_of_pending_connections() {
    let fixture = Fixture::new();
    let (_service, address, token) = open_session(&fixture);
    let quiet: Vec<_> = (0..PENDING_PEER_LIMIT).map(|_| stalled(&address)).collect();
    let mut extra = TcpStream::connect(&address).unwrap();
    extra
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let refused = extra.read(&mut [0; 1]);
    assert!(
        matches!(&refused, Ok(0))
            || refused.as_ref().is_err_and(|error| matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
            )),
        "{refused:?}"
    );
    drop(quiet);
    let response = exchange(
        &address,
        &page_request(&address, &token),
        Duration::from_secs(2),
    );
    assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
}

#[test]
fn pending_connections_are_bounded_across_peers() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let _clients: Vec<_> = (0..=PENDING_LIMIT)
        .map(|_| TcpStream::connect(address).unwrap())
        .collect();
    let mut pending = Pending::default();
    for index in 0..PENDING_LIMIT {
        let (stream, _) = listener.accept().unwrap();
        let peer = Ipv4Addr::new(192, 168, 1, index as u8 + 1);
        assert!(pending.admit(stream, peer.into()));
    }
    let (stream, _) = listener.accept().unwrap();
    assert!(!pending.admit(stream, Ipv4Addr::new(192, 168, 1, 200).into()));
}

#[test]
fn a_browser_loading_the_page_assets_in_parallel_gets_every_one() {
    let fixture = Fixture::new();
    let (_service, address, token) = open_session(&fixture);
    let routes = [
        "",
        "app.css",
        "app.js",
        "crop.js",
        "storage.js",
        "astro-mark.svg",
    ];
    let mut sockets: Vec<_> = routes
        .iter()
        .map(|_| TcpStream::connect(&address).unwrap())
        .collect();
    for (socket, route) in sockets.iter_mut().zip(routes) {
        write!(
            socket,
            "GET /{token}/{route} HTTP/1.1\r\nHost: {address}\r\n\r\n"
        )
        .unwrap();
    }
    for mut socket in sockets {
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut response = Vec::new();
        let _ = socket.read_to_end(&mut response);
        let response = String::from_utf8_lossy(&response);
        assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
    }
}

/// A loopback client bound to `source`, so tests can act as distinct LAN peers.
#[cfg(target_os = "linux")]
fn connect_from(source: Ipv4Addr, address: &str) -> TcpStream {
    use std::os::fd::FromRawFd;
    #[repr(C)]
    struct In {
        family: u16,
        port: u16,
        addr: [u8; 4],
        zero: [u8; 8],
    }
    unsafe extern "C" {
        fn socket(domain: i32, kind: i32, protocol: i32) -> i32;
        fn bind(fd: i32, address: *const In, length: u32) -> i32;
        fn connect(fd: i32, address: *const In, length: u32) -> i32;
    }
    let target: std::net::SocketAddrV4 = address.parse().unwrap();
    unsafe {
        let fd = socket(2, 1, 0);
        assert!(fd >= 0);
        let local = In {
            family: 2,
            port: 0,
            addr: source.octets(),
            zero: [0; 8],
        };
        assert_eq!(bind(fd, &local, 16), 0);
        let remote = In {
            family: 2,
            port: target.port().to_be(),
            addr: target.ip().octets(),
            zero: [0; 8],
        };
        assert_eq!(connect(fd, &remote, 16), 0);
        TcpStream::from_raw_fd(fd)
    }
}

#[cfg(target_os = "linux")]
fn stalled_from(source: Ipv4Addr, address: &str) -> TcpStream {
    let mut socket = connect_from(source, address);
    socket.write_all(b"GET /").unwrap();
    socket
}

fn was_refused(mut socket: TcpStream) -> bool {
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    match socket.read(&mut [0; 1]) {
        Ok(0) => true,
        Err(error) => matches!(
            error.kind(),
            std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
        ),
        Ok(_) => false,
    }
}

fn upload_bytes(address: &str, token: &str, body: &[u8]) -> Vec<u8> {
    let mut request = format!("POST /{token}/upload HTTP/1.1\r\nHost: {address}\r\nOrigin: http://{address}\r\nX-Astro-Upload: {token}\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
    request.extend_from_slice(body);
    request
}

fn parallel_page_load(address: &str, token: &str) {
    let routes = [
        "",
        "app.css",
        "app.js",
        "crop.js",
        "storage.js",
        "astro-mark.svg",
        "info.json",
    ];
    let mut sockets: Vec<_> = routes
        .iter()
        .map(|_| TcpStream::connect(address).unwrap())
        .collect();
    for (socket, route) in sockets.iter_mut().zip(routes) {
        write!(
            socket,
            "GET /{token}/{route} HTTP/1.1\r\nHost: {address}\r\n\r\n"
        )
        .unwrap();
    }
    for mut socket in sockets {
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut response = Vec::new();
        let _ = socket.read_to_end(&mut response);
        let response = String::from_utf8_lossy(&response);
        assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn peer_at_its_limit_leaves_another_peer_a_full_page_load_and_upload() {
    let fixture = Fixture::new();
    let (_service, address, token) = open_session(&fixture);
    let attacker = Ipv4Addr::new(127, 0, 0, 2);
    let quiet: Vec<_> = (0..PENDING_PEER_LIMIT)
        .map(|_| stalled_from(attacker, &address))
        .collect();
    thread::sleep(Duration::from_millis(200));
    assert!(was_refused(connect_from(attacker, &address)));
    let started = Instant::now();
    parallel_page_load(&address, &token);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    // Debug-build image processing alone takes ~5 s, so only the outcome is timed loosely.
    let response = exchange(
        &address,
        &upload_bytes(&address, &token, &image(1920, 1080)),
        Duration::from_secs(20),
    );
    assert!(response.starts_with("HTTP/1.1 201 "), "{response}");
    assert_eq!(catalog(&fixture.wallpapers()).len(), 1);
    drop(quiet);
}

#[cfg(target_os = "linux")]
#[test]
fn peer_reconnecting_its_stalls_past_header_time_never_blocks_the_owner() {
    let fixture = Fixture::new();
    let (_service, address, token) = open_session(&fixture);
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let attacker = {
        let address = address.clone();
        let stop = stop.clone();
        thread::spawn(move || {
            let source = Ipv4Addr::new(127, 0, 0, 3);
            let mut held: Vec<TcpStream> = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                held.retain(|socket| {
                    let mut probe = [0; 1];
                    socket.set_nonblocking(true).unwrap();
                    (&*socket).read(&mut probe).is_err()
                });
                while held.len() < 3 * PENDING_PEER_LIMIT {
                    held.push(stalled_from(source, &address));
                }
                thread::sleep(Duration::from_millis(20));
            }
        })
    };
    let until = Instant::now() + HEADER_TIME + Duration::from_secs(2);
    let mut served = 0;
    while Instant::now() < until {
        let started = Instant::now();
        let response = exchange(
            &address,
            &page_request(&address, &token),
            Duration::from_secs(2),
        );
        assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
        served += 1;
        thread::sleep(Duration::from_millis(300));
    }
    stop.store(true, Ordering::Relaxed);
    attacker.join().unwrap();
    assert!(served >= 15, "{served}");
}

#[test]
fn headers_one_byte_at_a_time_are_served() {
    let fixture = Fixture::new();
    let (_service, address, token) = open_session(&fixture);
    let mut socket = TcpStream::connect(&address).unwrap();
    socket.set_nodelay(true).unwrap();
    for byte in page_request(&address, &token) {
        socket.write_all(&[byte]).unwrap();
        thread::sleep(Duration::from_millis(5));
    }
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut response = String::new();
    let _ = socket.read_to_string(&mut response);
    assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
}

#[test]
fn headers_finished_just_before_their_deadline_are_served() {
    let fixture = Fixture::new();
    let (_service, address, token) = open_session(&fixture);
    let request = page_request(&address, &token);
    let mut socket = TcpStream::connect(&address).unwrap();
    socket.write_all(&request[..5]).unwrap();
    thread::sleep(HEADER_TIME - Duration::from_millis(700));
    socket.write_all(&request[5..]).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut response = String::new();
    let _ = socket.read_to_string(&mut response);
    assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
}

#[test]
fn trickled_headers_are_cut_off_at_header_time() {
    let fixture = Fixture::new();
    let (_service, address, _token) = open_session(&fixture);
    let socket = TcpStream::connect(&address).unwrap();
    socket.set_nodelay(true).unwrap();
    let mut writer = socket.try_clone().unwrap();
    let started = Instant::now();
    let trickle = thread::spawn(move || {
        for _ in 0..80 {
            if writer.write_all(b"a").is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    });
    let mut reader = socket;
    reader
        .set_read_timeout(Some(Duration::from_secs(9)))
        .unwrap();
    let mut response = Vec::new();
    let read = reader.read_to_end(&mut response);
    let elapsed = started.elapsed();
    let response = String::from_utf8_lossy(&response).into_owned();
    // The trickle keeps writing after the 400, so the close can race into a reset that discards
    // part of it; either way the connection must end at HEADER_TIME.
    match read {
        Ok(_) => assert!(response.contains("Request headers timed out"), "{response}"),
        Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset, "{error}"),
    }
    assert!(
        elapsed >= HEADER_TIME && elapsed < HEADER_TIME + Duration::from_secs(1),
        "{elapsed:?}"
    );
    trickle.join().unwrap();
}

#[test]
fn upload_with_headers_and_partial_body_in_one_write_is_stored_whole() {
    for split in [1, 5000, 30000] {
        let fixture = Fixture::new();
        let (_service, address, token) = open_session(&fixture);
        let body = image(1920, 1080);
        assert!(body.len() > split);
        let request = upload_bytes(&address, &token, &body);
        let headers = request.len() - body.len();
        let mut socket = TcpStream::connect(&address).unwrap();
        socket.write_all(&request[..headers + split]).unwrap();
        thread::sleep(Duration::from_millis(150));
        socket.write_all(&request[headers + split..]).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut response = String::new();
        let _ = socket.read_to_string(&mut response);
        assert!(
            response.starts_with("HTTP/1.1 201 "),
            "split {split}: {response}"
        );
        let items = catalog(&fixture.wallpapers());
        assert_eq!(items.len(), 1);
        let stored = image::open(items[0].directory.join("wallpaper.jpg")).unwrap();
        assert_eq!((stored.width(), stored.height()), (1920, 1080));
    }
}

#[test]
fn truncated_bodies_and_rejected_uploads_leave_the_gallery_unchanged() {
    let fixture = Fixture::new();
    let (_service, address, token) = open_session(&fixture);
    let body = image(1920, 1080);
    let request = upload_bytes(&address, &token, &body);
    let headers = request.len() - body.len();
    // Truncated body, then close.
    let mut socket = TcpStream::connect(&address).unwrap();
    socket.write_all(&request[..headers + 100]).unwrap();
    drop(socket);
    // Headers that never finish on a POST.
    let mut socket = TcpStream::connect(&address).unwrap();
    socket.write_all(&request[..headers - 2]).unwrap();
    thread::sleep(HEADER_TIME + Duration::from_millis(500));
    drop(socket);
    let foreign = String::from_utf8_lossy(&request).replacen(
        &format!("Host: {address}"),
        "Host: evil.example",
        1,
    );
    let response = exchange(&address, foreign.as_bytes(), Duration::from_secs(3));
    assert!(response.starts_with("HTTP/1.1 403 "), "{response}");
    assert!(catalog(&fixture.wallpapers()).is_empty());
    let response = exchange(
        &address,
        &page_request(&address, &token),
        Duration::from_secs(3),
    );
    assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
}

#[test]
fn connections_closed_mid_header_free_their_slots() {
    let fixture = Fixture::new();
    let (_service, address, token) = open_session(&fixture);
    for _ in 0..10 {
        let quiet: Vec<_> = (0..PENDING_PEER_LIMIT).map(|_| stalled(&address)).collect();
        thread::sleep(Duration::from_millis(60));
        drop(quiet);
        thread::sleep(Duration::from_millis(60));
    }
    let started = Instant::now();
    let response = exchange(
        &address,
        &page_request(&address, &token),
        Duration::from_secs(2),
    );
    assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn requests_with_the_token_still_end_the_session_at_the_cap() {
    let fixture = Fixture::new();
    let (service, address, token) = open_session(&fixture);
    let tokenless = format!("GET /{token}x/ HTTP/1.1\r\nHost: {address}\r\n\r\n");
    for index in 0..SESSION_REQUEST_LIMIT {
        for _ in 0..2 {
            let response = exchange(&address, tokenless.as_bytes(), Duration::from_secs(3));
            assert!(response.starts_with("HTTP/1.1 404 "), "{response}");
        }
        let request = if index % 2 == 0 {
            page_request(&address, &token)
        } else {
            format!("GET /{token} HTTP/1.1\r\nHost: {address}\r\n\r\n").into_bytes()
        };
        let response = exchange(&address, &request, Duration::from_secs(3));
        assert!(
            response.starts_with("HTTP/1.1 200 ") || response.starts_with("HTTP/1.1 303 "),
            "{index}: {response}"
        );
    }
    assert!(matches!(
        service
            .receiver
            .recv_timeout(Duration::from_secs(3))
            .unwrap(),
        Event::Ended(_)
    ));
    assert!(TcpStream::connect(&address).is_err());
}
