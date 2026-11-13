//! Finds the installed package that owns the running binary: the directory next to the
//! executable.
//!
//! Only that directory is accepted, never another installation of the same app, so a
//! broken package fails loudly instead of running with someone else's assets.
use std::{
    fs,
    path::{Path, PathBuf},
};

/// Reports whether both paths are the same regular file (same device and inode on Unix).
fn same_executable(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (fs::metadata(a), fs::metadata(b)) {
            (Ok(a), Ok(b)) => {
                a.is_file() && b.is_file() && a.dev() == b.dev() && a.ino() == b.ino()
            }
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        fs::canonicalize(a)
            .ok()
            .zip(fs::canonicalize(b).ok())
            .is_some_and(|(a, b)| a == b)
    }
}

/// Reports whether `directory` is Astro's native package for `executable`: it has `assets/`,
/// its `astro` is that very file, and its `appinfo.json` names Astro's ID, type `native` and
/// main `astro`.
fn owns_executable(directory: &Path, executable: &Path) -> bool {
    if !directory.join("assets").is_dir() || !same_executable(&directory.join("astro"), executable)
    {
        return false;
    }
    let manifest = fs::read(directory.join("appinfo.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    manifest.is_some_and(|manifest| {
        manifest.get("id").and_then(serde_json::Value::as_str) == Some(super::APP_ID)
            && manifest.get("type").and_then(serde_json::Value::as_str) == Some("native")
            && manifest.get("main").and_then(serde_json::Value::as_str) == Some("astro")
    })
}

/// Returns the package directory that contains `executable`.
///
/// # Errors
///
/// Returns a message naming the executable when its parent directory is not a complete
/// Astro native package (see `owns_executable`).
pub fn find_directory(executable: &Path) -> Result<PathBuf, String> {
    executable
        .parent()
        .map(Path::to_path_buf)
        .filter(|directory| owns_executable(directory, executable))
        .ok_or_else(|| {
            format!(
                "No installed native Astro package matches {}",
                executable.display()
            )
        })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "astro-install-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            Self(root)
        }

        fn install(&self, location: &str) -> PathBuf {
            let directory = self.0.join(location).join(super::super::APP_ID);
            fs::create_dir_all(directory.join("assets")).unwrap();
            fs::write(directory.join("astro"), b"native executable").unwrap();
            fs::write(
                directory.join("appinfo.json"),
                serde_json::json!({
                    "id": super::super::APP_ID, "type": "native", "main": "astro"
                })
                .to_string(),
            )
            .unwrap();
            directory
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn normal_launch_keeps_its_own_package_even_with_another_installation() {
        let fixture = Fixture::new();
        let developer = fixture.install("developer");
        let production = fixture.install("cryptofs");
        for directory in [&developer, &production] {
            assert_eq!(
                find_directory(&directory.join("astro")).unwrap(),
                *directory
            );
        }
    }

    #[test]
    fn incomplete_package_never_silently_uses_another_installation() {
        let fixture = Fixture::new();
        fixture.install("developer");
        let production = fixture.install("cryptofs");
        fs::remove_dir(production.join("assets")).unwrap();
        assert!(find_directory(&production.join("astro")).is_err());
    }

    #[test]
    fn package_requires_the_matching_native_package_manifest() {
        for manifest in [
            "not json",
            r#"{"id":"other","type":"native","main":"astro"}"#,
            r#"{"id":"com.danielcanoh22.astro","type":"web","main":"astro"}"#,
            r#"{"id":"com.danielcanoh22.astro","type":"native","main":"other"}"#,
        ] {
            let fixture = Fixture::new();
            let directory = fixture.install("cryptofs");
            fs::write(directory.join("appinfo.json"), manifest).unwrap();
            assert!(find_directory(&directory.join("astro")).is_err());
        }
    }
}
