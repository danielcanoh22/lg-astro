use crate::{
    APP, ROOTS, Result,
    system::{self, System},
    valid_app_id, valid_background, valid_upload,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs,
    path::{Component, Path, PathBuf},
};

fn resolve(directory: &Path, icon: &str) -> Option<PathBuf> {
    if icon.is_empty() || icon.len() > 4096 || icon.chars().any(char::is_control) {
        return None;
    }
    let icon = icon.strip_prefix("file://").unwrap_or(icon);
    if icon.contains("://") {
        return None;
    }
    let path = Path::new(icon);
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return None;
    }
    let path = if path.is_absolute() {
        path.into()
    } else {
        directory.join(path)
    };
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    (ROOTS.iter().any(|root| path.starts_with(root))
        && matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "webp"))
    .then_some(path)
}
#[derive(Deserialize)]
pub struct IconSource {
    pub id: String,
    pub directory: Option<String>,
    pub large_icon: Option<String>,
    pub icon: Option<String>,
}
pub fn read_icons(system: &impl System, source: &IconSource) -> Result<Value> {
    if !valid_app_id(&source.id) {
        return Err("Invalid application ID".into());
    }
    let mut directories: Vec<_> = ROOTS
        .iter()
        .map(|root| Path::new(root).join(&source.id))
        .collect();
    if let Some(directory) = &source.directory {
        let p = PathBuf::from(directory);
        if ROOTS.iter().any(|root| p.parent() == Some(Path::new(root)))
            && !p.components().any(|c| matches!(c, Component::ParentDir))
        {
            directories.retain(|old| old != &p);
            directories.insert(0, p);
        }
    }
    let mut remaining = 512 * 1024;
    let mut seen = HashSet::new();
    let mut icons = vec![];
    let roots = || ROOTS.iter().map(|root| system.path(Path::new(root)));
    for directory in directories {
        let manifest = crate::open_within(&system.path(&directory.join("appinfo.json")), roots())
            .and_then(|file| system::read_open(file, 64 * 1024).ok())
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .filter(|m| m["id"] == source.id)
            .unwrap_or(Value::Null);
        for icon in [
            source.large_icon.as_deref(),
            manifest["largeIcon"].as_str(),
            source.icon.as_deref(),
            manifest["icon"].as_str(),
            Some("icon.png"),
        ]
        .into_iter()
        .flatten()
        {
            let Some(path) = resolve(&directory, icon) else {
                continue;
            };
            if !seen.insert(path.clone()) {
                continue;
            }
            let Some(file) = crate::open_within(&system.path(&path), roots()) else {
                continue;
            };
            if let Ok(bytes) = system::read_open(file, remaining) {
                if bytes.is_empty() {
                    continue;
                }
                remaining -= bytes.len();
                let alphabet = b"0123456789abcdef";
                let mut hex = String::with_capacity(bytes.len() * 2);
                for byte in bytes {
                    hex.push(alphabet[(byte >> 4) as usize] as char);
                    hex.push(alphabet[(byte & 15) as usize] as char);
                }
                icons.push(hex);
            }
        }
    }
    Ok(json!({"returnValue":true,"icons":icons}))
}
fn installed(system: &impl System, directory: &Path) -> Result<Value> {
    if !ROOTS[..3]
        .iter()
        .any(|root| directory == Path::new(root).join(APP))
    {
        return Err("Invalid Astro installation directory".into());
    }
    let app = system::json(system, directory.join("appinfo.json"))?;
    if app["id"] != APP || app["type"] != "native" || app["main"] != "astro" {
        return Err("Requires an installed native Astro package".into());
    }
    Ok(app)
}
const SPLASH_LIMIT: usize = 32 * 1024 * 1024;

pub fn sync_splash(system: &impl System, directory: &Path, background: &str) -> Result<Value> {
    installed(system, directory)?;
    let image = if valid_upload(background) {
        // The first ranked storage that holds this upload, as native preferences select it.
        let storage = preference_files(system, directory)
            .iter()
            .filter_map(|path| storage_of(path, directory).map(Path::to_path_buf))
            .find(|storage| system::exists(system, storage.join("wallpapers").join(background)))
            .ok_or("Uploaded background unavailable")?;
        uploaded_splash(system, &storage, background)?
    } else if valid_background(background) {
        system::read(
            system,
            directory.join(format!("splash/{background}.png")),
            SPLASH_LIMIT,
        )?
    } else {
        return Err("Invalid splash background".into());
    };
    replace(system, &directory.join("splash.png"), &image)?;
    let config_path = Path::new("/var/lib/astro-home/state.json");
    if system::exists(system, config_path) {
        let config = system::json(system, config_path)?;
        if config["enabled"] == true
            && config["method"] != "executable"
            && config["appDir"].as_str() == directory.to_str()
        {
            replace(
                system,
                Path::new("/var/lib/astro-home/app/splash.png"),
                &image,
            )?;
        }
    }
    Ok(json!({"returnValue":true,"background":background}))
}
fn replace(system: &impl System, path: &Path, image: &[u8]) -> Result<()> {
    if system::read(system, path, SPLASH_LIMIT).is_ok_and(|bytes| bytes == image) {
        return Ok(());
    }
    system::write(system, path, image, 0o644)
}
fn real_directory(system: &impl System, path: &Path) -> Result<fs::Metadata> {
    let metadata =
        fs::symlink_metadata(system.path(path)).map_err(|e| format!("{}: {e}", path.display()))?;
    if !metadata.file_type().is_dir() {
        return Err(format!("{}: not a directory", path.display()));
    }
    Ok(metadata)
}
/// The 1920x1080 PNG Astro generated for an uploaded background, read as root from app-owned
/// storage. No path component may be a symlink, and whatever was opened must be that same
/// regular file, owned by the storage directory's owner: a path swapped during the checks can
/// only yield a file that owner could already read, never a root-only file.
fn uploaded_splash(system: &impl System, storage: &Path, id: &str) -> Result<Vec<u8>> {
    if !valid_upload(id) {
        return Err("Invalid uploaded background".into());
    }
    let owner = real_directory(system, storage)?;
    let gallery = storage.join("wallpapers");
    real_directory(system, &gallery)?;
    let folder = gallery.join(id);
    real_directory(system, &folder)?;
    let path = system.path(&folder.join("splash.png"));
    let linked = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
    if !linked.file_type().is_file() {
        return Err("Uploaded splash is not a regular file".into());
    }
    let file = fs::File::open(&path).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let opened = file.metadata().map_err(|e| e.to_string())?;
        if (opened.dev(), opened.ino()) != (linked.dev(), linked.ino())
            || opened.uid() != owner.uid()
        {
            return Err("Uploaded splash changed or has another owner".into());
        }
    }
    let _ = owner;
    // A directory component swapped for a symlink after its own check would redirect the
    // open: ask where the opened file really is. The storage's parent is a system directory
    // that may itself be reached through a symlink, so it is compared resolved.
    let parent = storage.parent().ok_or("Invalid storage directory")?;
    let expected = fs::canonicalize(system.path(parent))
        .map_err(|e| e.to_string())?
        .join(storage.file_name().ok_or("Invalid storage directory")?)
        .join("wallpapers")
        .join(id)
        .join("splash.png");
    if !opened_at(&file, &expected) {
        return Err("Uploaded splash moved while it was opened".into());
    }
    let bytes = system::read_open(file, SPLASH_LIMIT)?;
    let png = bytes.len() >= 24
        && bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        && &bytes[12..16] == b"IHDR"
        && bytes[16..20] == 1920u32.to_be_bytes()
        && bytes[20..24] == 1080u32.to_be_bytes();
    if !png {
        return Err("Uploaded splash is not a 1920x1080 PNG".into());
    }
    Ok(bytes)
}
/// Whether an opened file is the one at `path` (an absolute path without symlinks), according
/// to the kernel. Elsewhere than Linux there is no such record, and this only accepts.
pub(crate) fn opened_at(file: &fs::File, path: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
            .is_ok_and(|opened| opened == path)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (file, path);
        true
    }
}
/// The storage directory a `preferences.json` candidate belongs to; never the package itself,
/// whose legacy preferences file does not make it a place for uploaded backgrounds.
fn storage_of<'a>(preferences: &'a Path, directory: &Path) -> Option<&'a Path> {
    (preferences != directory.join("preferences.json"))
        .then(|| preferences.parent())
        .flatten()
}
/// Ranked `preferences.json` candidates: the same app-owned storage precedence as native
/// preferences, root-owned directories after the others, symlinked storage directories never
/// (this runs as root), then the package-local file older native builds used.
fn preference_files(system: &impl System, directory: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![
        format!("/media/developer/{APP}/preferences.json"),
        format!("/media/developer/temp/{APP}/preferences.json"),
        format!("/media/internal/.{APP}/preferences.json"),
    ];
    if directory.starts_with("/media/cryptofs") {
        candidates.rotate_right(1);
    }
    let mut ranked = vec![];
    for (index, path) in candidates.into_iter().enumerate() {
        let parent = Path::new(&path).parent().unwrap();
        if let Ok(meta) = fs::symlink_metadata(system.path(parent))
            && meta.is_dir()
            && system::exists(system, &path)
        {
            #[cfg(unix)]
            let root_owned = {
                use std::os::unix::fs::MetadataExt;
                meta.uid() == 0
            };
            #[cfg(not(unix))]
            let root_owned = false;
            ranked.push((root_owned, index, path));
        }
    }
    ranked.sort_by_key(|(root, index, _)| (*root, *index));
    let mut candidates: Vec<_> = ranked
        .into_iter()
        .map(|(_, _, path)| PathBuf::from(path))
        .collect();
    candidates.push(directory.join("preferences.json"));
    candidates
}
/// Select the same app-owned storage precedence as native preferences; never follow a storage
/// directory symlink while running as root. Older native builds also saved splash_background.
/// An uploaded splash comes from the storage whose preferences chose it; if it is unusable, the
/// package's current splash.png (normally the same image, synced earlier) is staged instead.
pub(crate) fn stage_splash(system: &impl System, directory: &Path) -> Result<()> {
    let (preferences, background) = preference_files(system, directory)
        .into_iter()
        .find_map(|path| {
            system::json(system, &path).ok().and_then(|value| {
                value
                    .get("splash_background")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .or_else(|| value.get("background").and_then(Value::as_str))
                    .filter(|s| valid_background(s) || valid_upload(s))
                    .map(|s| (Some(path.clone()), s.to_owned()))
            })
        })
        .unwrap_or_else(|| (None, "dunes".into()));
    let fallback = || system::read(system, directory.join("splash.png"), SPLASH_LIMIT);
    let bytes = if valid_upload(&background) {
        preferences
            .as_deref()
            .and_then(|preferences| storage_of(preferences, directory))
            .ok_or_else(|| "Missing storage".to_string())
            .and_then(|storage| uploaded_splash(system, storage, &background))
            .or_else(|_| fallback())?
    } else {
        let path = directory.join(format!("splash/{background}.png"));
        if system::exists(system, &path) {
            system::read(system, path, SPLASH_LIMIT)?
        } else {
            fallback()?
        }
    };
    system::write(system, "/var/lib/astro-home/app/splash.png", &bytes, 0o644)
}
