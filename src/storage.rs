use crate::model::Preferences;
use std::{
    fs,
    path::{Path, PathBuf},
};

pub fn default_path(webos: bool) -> Result<PathBuf, String> {
    if webos {
        #[cfg(all(feature = "webos", unix))]
        return tv_path();
        #[cfg(not(all(feature = "webos", unix)))]
        return Err("TV storage requires a webOS build".into());
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".config")))
        .ok_or("Set HOME, XDG_CONFIG_HOME or --preferences for native preferences")?;
    Ok(base.join("astro-native/preferences.json"))
}

#[cfg(all(feature = "webos", unix))]
fn tv_path() -> Result<PathBuf, String> {
    // The executable takeover runs at LG's path, but must retain the source
    // installation's storage precedence and installation-local migration path.
    let installation = crate::platform::install_dir()?;
    let legacy = installation.join("preferences.json");
    let id = crate::platform::APP_ID;
    // These volumes persist across boots. The app-specific directory also
    // survives replacement of the installed payload on an ordinary update.
    // Production native jails expose internal storage instead of developer.
    let developer = PathBuf::from("/media/developer")
        .join(id)
        .join("preferences.json");
    let shared_developer = PathBuf::from("/media/developer/temp")
        .join(id)
        .join("preferences.json");
    let internal = PathBuf::from("/media/internal")
        .join(format!(".{id}"))
        .join("preferences.json");
    let candidates = if installation.starts_with("/media/cryptofs") {
        [internal, developer, shared_developer]
    } else {
        [developer, shared_developer, internal]
    };
    tv_path_in(candidates, &legacy, unsafe { libc::geteuid() })
}

#[cfg(all(feature = "webos", unix))]
fn storage_rank(existing: bool, owner: Option<u32>, user: u32) -> (bool, bool) {
    // Root Home must prefer the app's existing settings over a root-owned
    // fallback left by an earlier takeover. Never merge or overwrite either.
    (!existing, user == 0 && owner == Some(0))
}

#[cfg(all(feature = "webos", unix))]
fn tv_path_in(mut candidates: [PathBuf; 3], legacy: &Path, user: u32) -> Result<PathBuf, String> {
    use std::{
        io::Write,
        os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    };

    // Keep existing settings ahead of new destinations, including after a reboot.
    candidates.sort_by_key(|path| {
        let owner = path
            .parent()
            .and_then(|directory| fs::symlink_metadata(directory).ok())
            .filter(|metadata| metadata.is_dir())
            .map(|metadata| metadata.uid());
        storage_rank(path.exists(), owner, user)
    });
    let mut failures = Vec::new();
    for path in candidates {
        let directory = path.parent().ok_or("Invalid TV storage directory")?;
        // Create only this app's leaf, not a missing system mount hierarchy.
        let created = fs::DirBuilder::new().mode(0o700).create(directory);
        if let Err(error) = created
            && error.kind() != std::io::ErrorKind::AlreadyExists
        {
            failures.push(format!("{}: {error}", directory.display()));
            continue;
        }
        let metadata = match fs::symlink_metadata(directory) {
            Ok(metadata) => metadata,
            Err(error) => {
                failures.push(format!("{}: {error}", directory.display()));
                continue;
            }
        };
        // Home runs as root rather than the jailed app user. It may reuse the
        // app's directory, but a normal app must still reject foreign owners.
        // symlink_metadata keeps symlink directories rejected in both modes.
        if !metadata.is_dir() || (user != 0 && metadata.uid() != user) {
            failures.push(format!(
                "{} is not owned by the app user",
                directory.display()
            ));
            continue;
        }
        let probe = directory.join(format!(".write-probe-{}", std::process::id()));
        let writable = (|| {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&probe)?;
            let result = file
                .write_all(b"Astro storage probe")
                .and_then(|_| file.sync_all());
            drop(file);
            // Only remove a probe that this process created successfully.
            let cleanup = fs::remove_file(&probe);
            result.and(cleanup)
        })();
        if let Err(error) = writable {
            failures.push(format!("{}: {error}", directory.display()));
            continue;
        }
        if !path.exists() {
            // Import JSON only from the old path in THIS installation, never
            // browser storage or another app ID's preferences.
            match fs::read(legacy) {
                Ok(bytes) => {
                    let mut preferences: Preferences =
                        serde_json::from_slice(&bytes).map_err(|e| {
                            format!(
                                "Existing preferences {} could not be read: {e}",
                                legacy.display()
                            )
                        })?;
                    preferences.sanitize();
                    save(&path, &preferences)?;
                    eprintln!(
                        "Astro preferences migrated from {} to {}",
                        legacy.display(),
                        path.display()
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "Existing preferences {} could not be read: {error}",
                        legacy.display()
                    ));
                }
            }
        }
        eprintln!("Astro persistent storage: {}", directory.display());
        return Ok(path);
    }
    Err(format!(
        "No writable persistent TV storage found: {}",
        failures.join("; ")
    ))
}

pub fn load(path: &Path) -> Result<Preferences, String> {
    match fs::read(path) {
        Ok(bytes) => {
            let mut preferences: Preferences = serde_json::from_slice(&bytes)
                .map_err(|e| format!("Preferences could not be read: {e}"))?;
            preferences.sanitize();
            Ok(preferences)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Preferences::default()),
        Err(e) => Err(format!("Preferences could not be read: {e}")),
    }
}

/// A preferences file's modification time and size, to notice a save by the other Astro copy
/// (Home and the TV-input app share one file).
pub type Stamp = Option<(std::time::SystemTime, u64)>;

pub fn stamp(path: &Path) -> Stamp {
    let metadata = fs::metadata(path).ok()?;
    Some((metadata.modified().ok()?, metadata.len()))
}

pub fn save(path: &Path, preferences: &Preferences) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(preferences)
        .map_err(|e| format!("Could not encode preferences: {e}"))?;
    write_atomic(path, &bytes)
}

/// The outcome of one background preferences write. `report` is set when any snapshot it
/// wrote came from a user action, whose failure the user is told about.
pub struct Saved {
    pub result: Result<(), String>,
    pub report: bool,
}

#[derive(Default)]
struct SaveQueue {
    next: Option<(Vec<u8>, bool)>,
    writing: bool,
    stop: bool,
}

/// Writes preferences on a worker thread, so the fsync of an atomic save never stalls the SDL
/// thread. Each save encodes the whole preferences at the call, so only the newest pending
/// snapshot is written: an older one that has not started is superseded. Writes keep call order,
/// `flush` waits for every pending write, and dropping the saver writes what is still pending.
pub struct Saver {
    path: PathBuf,
    queue: std::sync::Arc<(std::sync::Mutex<SaveQueue>, std::sync::Condvar)>,
    sender: std::sync::mpsc::Sender<Saved>,
    results: std::sync::mpsc::Receiver<Saved>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Saver {
    pub fn new(path: PathBuf) -> Self {
        let queue = std::sync::Arc::new((
            std::sync::Mutex::new(SaveQueue::default()),
            std::sync::Condvar::new(),
        ));
        let (sender, results) = std::sync::mpsc::channel();
        let worker = {
            let (queue, path, sender) = (queue.clone(), path.clone(), sender.clone());
            std::thread::Builder::new()
                .name("astro-preferences".into())
                .spawn(move || {
                    let (lock, ready) = &*queue;
                    loop {
                        let (bytes, report) = {
                            let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
                            loop {
                                // A pending snapshot is written before stopping.
                                if let Some(next) = state.next.take() {
                                    state.writing = true;
                                    break next;
                                }
                                if state.stop {
                                    return;
                                }
                                state = ready.wait(state).unwrap_or_else(|e| e.into_inner());
                            }
                        };
                        let result = write_atomic(&path, &bytes);
                        let _ = sender.send(Saved { result, report });
                        lock.lock().unwrap_or_else(|e| e.into_inner()).writing = false;
                        ready.notify_all();
                        crate::wake::wake();
                    }
                })
                .ok()
        };
        if worker.is_none() {
            eprintln!("Could not start the preferences writer; saving on the UI thread");
        }
        Self {
            path,
            queue,
            sender,
            results,
            worker,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Queues the current preferences. An encoding error returns at once; a write error
    /// arrives later through `results`.
    pub fn save(&self, preferences: &Preferences, report: bool) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(preferences)
            .map_err(|e| format!("Could not encode preferences: {e}"))?;
        if self.worker.is_none() {
            let result = write_atomic(&self.path, &bytes);
            let _ = self.sender.send(Saved { result, report });
            return Ok(());
        }
        let (lock, ready) = &*self.queue;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        let report = report || state.next.as_ref().is_some_and(|(_, earlier)| *earlier);
        state.next = Some((bytes, report));
        ready.notify_all();
        Ok(())
    }

    /// Waits until every queued snapshot is on disk (or failed).
    pub fn flush(&self) {
        if self.worker.is_none() {
            return;
        }
        let (lock, ready) = &*self.queue;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        while state.next.is_some() || state.writing {
            state = ready.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Drains the results of finished writes, oldest first.
    pub fn results(&self) -> Vec<Saved> {
        self.results.try_iter().collect()
    }
}

impl Drop for Saver {
    fn drop(&mut self) {
        let (lock, ready) = &*self.queue;
        lock.lock().unwrap_or_else(|e| e.into_inner()).stop = true;
        ready.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Deletes the forecast cache earlier versions kept beside the preferences file
/// (`preferences.weather.json`), now that the weather widget is gone. A symlink is unlinked,
/// never followed; anything that is not a regular file or a link is left alone.
pub fn remove_weather_cache(preferences: &Path) {
    let cache = preferences.with_extension("weather.json");
    let Ok(metadata) = fs::symlink_metadata(&cache) else {
        return;
    };
    if !(metadata.is_file() || metadata.file_type().is_symlink()) {
        return;
    }
    if let Err(error) = fs::remove_file(&cache) {
        eprintln!(
            "Could not remove the old weather cache {}: {error}",
            cache.display()
        );
    }
}

/// The effective user, or `u32::MAX` (never root) where ownership is not managed.
pub fn effective_user() -> u32 {
    #[cfg(all(feature = "webos", unix))]
    return unsafe { libc::geteuid() };
    #[cfg(not(all(feature = "webos", unix)))]
    u32::MAX
}

/// Creates `directory` and its missing ancestors, like `fs::create_dir_all`. When root Home
/// (`user` 0) creates one, it takes the owner of the directory it was created in, so the normal
/// app can still use it after Home is disabled. The owner change never follows a symlink, and
/// existing directories keep their owner. The steps follow `create_dir_all`'s order (the whole
/// path first), so every outcome and error is the same as before.
pub fn create_directories(directory: &Path, user: u32) -> std::io::Result<()> {
    if directory.as_os_str().is_empty() {
        return Ok(());
    }
    match fs::create_dir(directory) {
        Ok(()) => return inherit_parent_owner(directory, user),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) if directory.is_dir() => return Ok(()),
        Err(error) => return Err(error),
    }
    match directory.parent() {
        Some(parent) => create_directories(parent, user)?,
        None => return Err(std::io::Error::other("failed to create whole tree")),
    }
    match fs::create_dir(directory) {
        Ok(()) => inherit_parent_owner(directory, user),
        Err(_) if directory.is_dir() => Ok(()),
        Err(error) => Err(error),
    }
}

/// Gives a directory root Home just created the owner of the directory it was created in.
fn inherit_parent_owner(directory: &Path, user: u32) -> std::io::Result<()> {
    #[cfg(all(feature = "webos", unix))]
    if user == 0 {
        use std::os::unix::fs::MetadataExt;
        let parent = directory
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let owner = fs::metadata(parent)?;
        std::os::unix::fs::lchown(directory, Some(owner.uid()), Some(owner.gid()))?;
    }
    let _ = (directory, user);
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::{
        io::Write,
        sync::atomic::{AtomicUsize, Ordering},
    };

    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        create_directories(parent, effective_user()).map_err(|e| {
            format!(
                "Could not prepare preferences directory {}: {e}",
                parent.display()
            )
        })?;
    }
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let temporary = path.with_extension(format!(
        "json.tmp-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    // Never follow an app-created temporary symlink while running as root.
    let mut file = options
        .open(&temporary)
        .map_err(|e| format!("Could not create {}: {e}", temporary.display()))?;
    let result = (|| {
        file.write_all(bytes)?;
        #[cfg(all(feature = "webos", unix))]
        if unsafe { libc::geteuid() } == 0 {
            use std::os::{fd::AsRawFd, unix::fs::MetadataExt};
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let metadata = fs::symlink_metadata(parent)?;
            if !metadata.is_dir() {
                return Err(std::io::Error::other(
                    "Storage directory is not a real directory",
                ));
            }
            // Atomic replacement must not turn private app files into root-only
            // files: disabling Home should leave the normal app able to read them.
            if unsafe { libc::fchown(file.as_raw_fd(), metadata.uid(), metadata.gid()) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        file.sync_all()?;
        fs::rename(&temporary, path)
    })();
    drop(file);
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|e| format!("Could not save {}: {e}", path.display()))
}

#[cfg(all(test, feature = "webos", unix))]
mod tests {
    use super::*;
    use std::{
        os::unix::fs::{MetadataExt, PermissionsExt, symlink},
        sync::atomic::{AtomicUsize, Ordering},
    };

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "astro-storage-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            Self(root)
        }

        fn candidates(&self) -> [PathBuf; 3] {
            ["developer", "shared", "internal"]
                .map(|name| self.0.join(name).join("preferences.json"))
        }

        fn legacy(&self) -> PathBuf {
            self.0.join("preferences.json")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn root_home_reuses_each_existing_app_storage_destination() {
        for index in 0..3 {
            let fixture = Fixture::new();
            let candidates = fixture.candidates();
            let existing = &candidates[index];
            let preferences = Preferences {
                background: "forest".into(),
                favorites: Some(vec!["youtube.leanback.v4".into()]),
                ..Preferences::default()
            };
            save(existing, &preferences).unwrap();
            let original = fs::read(existing).unwrap();
            // Model root Home on a host-owned directory without requiring sudo.
            let selected = tv_path_in(candidates.clone(), &fixture.legacy(), 0).unwrap();
            assert_eq!(&selected, existing);
            assert_eq!(load(&selected).unwrap().background, "forest");
            assert_eq!(fs::read(existing).unwrap(), original);
            for (other, path) in candidates.iter().enumerate() {
                if other != index {
                    assert!(!path.parent().unwrap().exists());
                }
            }
            let normal =
                tv_path_in(candidates, &fixture.legacy(), unsafe { libc::geteuid() }).unwrap();
            assert_eq!(normal, selected);
        }
    }

    #[test]
    fn created_directories_match_create_dir_all_outcomes_and_errors() {
        // Each case builds the same tree twice and compares the outcome of both functions.
        type Setup = fn(&Path);
        let cases: [(&str, Setup, &str); 8] = [
            (
                "existing",
                |r| fs::create_dir_all(r.join("a/b")).unwrap(),
                "a/b",
            ),
            ("nested missing", |_| {}, "a/b/c/d"),
            (
                "symlinked ancestor",
                |r| {
                    fs::create_dir(r.join("real")).unwrap();
                    symlink(r.join("real"), r.join("link")).unwrap();
                },
                "link/x/y",
            ),
            (
                "symlink leaf to dir",
                |r| {
                    fs::create_dir(r.join("real")).unwrap();
                    symlink(r.join("real"), r.join("link")).unwrap();
                },
                "link",
            ),
            (
                "dangling symlink ancestor",
                |r| symlink(r.join("nowhere"), r.join("link")).unwrap(),
                "link/x",
            ),
            (
                "file as parent",
                |r| fs::write(r.join("file"), b"").unwrap(),
                "file/x",
            ),
            (
                "file as ancestor",
                |r| fs::write(r.join("file"), b"").unwrap(),
                "file/x/y",
            ),
            (
                "file as leaf",
                |r| fs::write(r.join("file"), b"").unwrap(),
                "file",
            ),
        ];
        let fixture = Fixture::new();
        let mut mismatches = Vec::new();
        for (index, (name, setup, target)) in cases.into_iter().enumerate() {
            let std_root = fixture.0.join(format!("std-{index}"));
            let new_root = fixture.0.join(format!("new-{index}"));
            for root in [&std_root, &new_root] {
                fs::create_dir(root).unwrap();
                setup(root);
            }
            let expected = fs::create_dir_all(std_root.join(target)).map_err(|e| e.to_string());
            for user in [0, unsafe { libc::geteuid() }, u32::MAX] {
                let _ = fs::remove_dir_all(&new_root);
                fs::create_dir(&new_root).unwrap();
                setup(&new_root);
                let actual =
                    create_directories(&new_root.join(target), user).map_err(|e| e.to_string());
                if actual != expected {
                    mismatches.push(format!("{name} (user {user}): create_dir_all {expected:?}, create_directories {actual:?}"));
                }
                if expected.is_ok() {
                    assert!(new_root.join(target).is_dir(), "{name}");
                }
            }
        }
        assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
    }

    #[test]
    fn concurrent_directory_creation_all_succeeds() {
        let fixture = Fixture::new();
        let target = fixture.0.join("a/b/c/d/e/f");
        for round in 0..20 {
            let target = target.join(round.to_string());
            let threads: Vec<_> = (0..8)
                .map(|_| {
                    let target = target.clone();
                    std::thread::spawn(move || create_directories(&target, 0))
                })
                .collect();
            for thread in threads {
                thread.join().unwrap().unwrap();
            }
            assert!(target.is_dir());
        }
    }

    #[test]
    fn root_home_keeps_an_existing_directorys_owner_and_mode() {
        // Under `unshare -r` this runs the real euid-0 path; otherwise it models root Home.
        let fixture = Fixture::new();
        let existing = fixture.0.join("existing");
        fs::create_dir(&existing).unwrap();
        fs::set_permissions(&existing, fs::Permissions::from_mode(0o750)).unwrap();
        let before = fs::symlink_metadata(&existing).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        create_directories(&existing, 0).unwrap();
        let after = fs::symlink_metadata(&existing).unwrap();
        assert_eq!(
            (
                before.uid(),
                before.gid(),
                before.mode(),
                before.ctime(),
                before.ctime_nsec()
            ),
            (
                after.uid(),
                after.gid(),
                after.mode(),
                after.ctime(),
                after.ctime_nsec()
            ),
            "an existing directory's inode was changed"
        );
        create_directories(&existing.join("child"), 0).unwrap();
        let child = fs::symlink_metadata(existing.join("child")).unwrap();
        assert_eq!((child.uid(), child.gid()), (before.uid(), before.gid()));
    }

    #[test]
    fn a_file_in_place_of_the_preferences_directory_keeps_its_save_errors() {
        let fixture = Fixture::new();
        fs::write(fixture.0.join("file"), b"").unwrap();
        let parent = save(
            &fixture.0.join("file/preferences.json"),
            &Preferences::default(),
        );
        assert!(parent.unwrap_err().ends_with("File exists (os error 17)"));
        let ancestor = save(
            &fixture.0.join("file/below/preferences.json"),
            &Preferences::default(),
        );
        assert!(
            ancestor
                .unwrap_err()
                .ends_with("Not a directory (os error 20)")
        );
    }

    #[test]
    fn directories_root_home_creates_take_the_owner_of_the_directory_they_are_created_in() {
        let fixture = Fixture::new();
        let nested = fixture.0.join("new").join("nested");
        // Model root Home on a host-owned directory without requiring sudo.
        create_directories(&nested, 0).unwrap();
        let owner = fs::metadata(&fixture.0).unwrap();
        for directory in [fixture.0.join("new"), nested.clone()] {
            let metadata = fs::symlink_metadata(&directory).unwrap();
            assert!(metadata.is_dir());
            assert_eq!((metadata.uid(), metadata.gid()), (owner.uid(), owner.gid()));
        }
        // Existing directories are accepted as they are, for root and the normal app.
        create_directories(&nested, 0).unwrap();
        create_directories(&nested, unsafe { libc::geteuid() }).unwrap();
        // Something other than a directory in the way still fails.
        fs::write(fixture.0.join("file"), b"").unwrap();
        assert!(create_directories(&fixture.0.join("file").join("below"), 0).is_err());
        // A save creates its missing directory the same way.
        let preferences = fixture.0.join("saved").join("preferences.json");
        save(&preferences, &Preferences::default()).unwrap();
        let metadata = fs::symlink_metadata(preferences.parent().unwrap()).unwrap();
        assert_eq!((metadata.uid(), metadata.gid()), (owner.uid(), owner.gid()));
    }

    #[test]
    fn existing_app_preferences_rank_before_root_fallbacks_without_changing_normal_order() {
        assert!(storage_rank(true, Some(5000), 0) < storage_rank(true, Some(0), 0));
        assert!(storage_rank(true, Some(0), 0) < storage_rank(false, Some(5000), 0));
        assert_eq!(
            storage_rank(true, Some(5000), 5000),
            storage_rank(true, Some(0), 5000)
        );
    }

    #[test]
    fn normal_app_still_rejects_foreign_owned_directories() {
        let fixture = Fixture::new();
        let candidates = fixture.candidates();
        let foreign_user = unsafe { libc::geteuid() } + 1;
        let error = tv_path_in(candidates.clone(), &fixture.legacy(), foreign_user).unwrap_err();
        assert!(error.contains("not owned by the app user"));
        assert!(candidates.iter().all(|path| !path.exists()));
    }

    #[test]
    fn root_and_normal_app_both_reject_symlink_storage_directories() {
        for user in [0, unsafe { libc::geteuid() }] {
            let fixture = Fixture::new();
            let candidates = fixture.candidates();
            save(&candidates[1], &Preferences::default()).unwrap();
            symlink(
                candidates[1].parent().unwrap(),
                candidates[0].parent().unwrap(),
            )
            .unwrap();
            assert_eq!(
                tv_path_in(candidates.clone(), &fixture.legacy(), user).unwrap(),
                candidates[1]
            );
        }
    }

    #[test]
    fn existing_corrupt_app_preferences_are_not_replaced_with_defaults() {
        let fixture = Fixture::new();
        let candidates = fixture.candidates();
        fs::create_dir(candidates[1].parent().unwrap()).unwrap();
        fs::write(&candidates[1], "broken").unwrap();
        let selected = tv_path_in(candidates.clone(), &fixture.legacy(), 0).unwrap();
        assert_eq!(selected, candidates[1]);
        assert!(load(&selected).is_err());
        assert_eq!(fs::read_to_string(&selected).unwrap(), "broken");
    }

    #[test]
    fn root_home_can_still_import_installation_local_preferences_once() {
        let fixture = Fixture::new();
        let candidates = fixture.candidates();
        save(
            &fixture.legacy(),
            &Preferences {
                background: "forest".into(),
                ..Preferences::default()
            },
        )
        .unwrap();
        let selected = tv_path_in(candidates.clone(), &fixture.legacy(), 0).unwrap();
        assert_eq!(selected, candidates[0]);
        assert_eq!(load(&selected).unwrap().background, "forest");
        fs::write(fixture.legacy(), "broken").unwrap();
        assert_eq!(
            tv_path_in(candidates, &fixture.legacy(), 0).unwrap(),
            selected
        );
        assert_eq!(load(&selected).unwrap().background, "forest");
    }

    #[test]
    fn atomic_saves_keep_private_app_owned_files_and_clean_up_failed_writes() {
        let fixture = Fixture::new();
        let preferences = fixture.legacy();
        for bytes in [b"first".as_slice(), b"second".as_slice()] {
            write_atomic(&preferences, bytes).unwrap();
            let owner = fs::metadata(&fixture.0).unwrap();
            let saved = fs::metadata(&preferences).unwrap();
            assert_eq!((saved.uid(), saved.gid()), (owner.uid(), owner.gid()));
            assert_eq!(saved.permissions().mode() & 0o777, 0o600);
            assert_eq!(fs::read(&preferences).unwrap(), bytes);
            assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 1);
        }
        let blocked = fixture.0.join("blocked.json");
        fs::create_dir(&blocked).unwrap();
        assert!(write_atomic(&blocked, b"cannot replace a directory").is_err());
        assert!(blocked.is_dir());
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 2);
        assert_eq!(fs::read(&preferences).unwrap(), b"second");
    }
}
