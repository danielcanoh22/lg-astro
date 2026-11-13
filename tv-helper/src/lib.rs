//! Privileged, bounded TV operations for Astro: the library behind the `astro-helper` executable.
//!
//! The helper does the few things Astro cannot do from its app jail: read other apps' icons,
//! write the package's splash image, uninstall apps, register Astro as a TV input and manage the
//! optional Home integration. It runs as root in two ways:
//!
//! - As the `astro-helper` binary, executed by Homebrew Channel's root exec service with
//!   individually quoted arguments, or by `tv/home.sh` (root over SSH, the boot hook, or the
//!   fallback systemd unit).
//! - In process, when Astro already runs as root as Home: [`home::record_start`],
//!   [`home::record_healthy`] and [`files::sync_splash`] are called directly.
//!
//! Either way it acts with root privileges on requests an unprivileged app could have shaped, so
//! it never trusts caller paths. IDs are checked against fixed grammars ([`valid_app_id`],
//! [`valid_background`], [`valid_upload`]); files are read only below fixed roots, through
//! [`open_within`] or no-follow and owner checks; and every read, child process and reply stream
//! is bounded in size and time. All system access goes through [`system::System`], so tests run
//! the same code against a fixture root.
//!
//! No SDL, image decoding, browser engine or persistent daemon is involved.
pub mod files;
pub mod home;
pub mod input;
pub mod system;
pub mod uninstall;

/// Result of a helper operation. Errors are human-readable messages for the user or the log.
pub type Result<T> = std::result::Result<T, String>;
/// Astro's own app ID.
pub const APP: &str = "com.danielcanoh22.astro";
/// Directories where webOS installs applications, one subdirectory per app ID.
///
/// Developer-mode installs, Astro included, live under the first; the others hold apps from the
/// store or the firmware. Files read on a caller's behalf must resolve below one of these.
pub const ROOTS: [&str; 5] = [
    "/media/developer/apps/usr/palm/applications",
    "/media/cryptofs/apps/usr/palm/applications",
    "/usr/palm/applications",
    "/media/system/apps/usr/palm/applications",
    "/mnt/otncabi/usr/palm/applications",
];
/// Whether `id` has the shape of a webOS app ID: at most 128 bytes of ASCII alphanumeric
/// segments joined by `.`, `_` or `-`, none of them empty.
///
/// Such an ID contains no `/` and cannot be `.` or `..`, so it is safe to join onto a root.
pub fn valid_app_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .split(['.', '_', '-'])
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric()))
}
/// Whether `id` is in an LG or Palm system namespace. Such apps are never removable and are
/// hidden from Astro's app list unless their manifest marks them visible.
pub fn vendor_app(id: &str) -> bool {
    ["com.webos.", "com.palm.", "com.lge."]
        .iter()
        .any(|prefix| id.starts_with(prefix))
}
/// Whether an application may be uninstalled: a valid ID outside vendor namespaces that is
/// neither Astro nor Homebrew Channel. Identities only, never titles.
pub fn removable(id: &str) -> bool {
    valid_app_id(id)
        && !vendor_app(id)
        && ![
            APP,
            "org.webosbrew.hbchannel",
            "org.webosbrew.hbchannel.service",
        ]
        .contains(&id)
}
/// Opens `path` only if, with every symlink resolved, it is a regular file inside one of
/// `roots`. A path is otherwise only checked as text, so a link inside an application's
/// directory could name any file on the TV, or a FIFO or device that stalls a reader.
///
/// Roots that do not exist are ignored. On Linux the opened descriptor is checked again through
/// `/proc/self/fd`, so a symlink swapped in between the check and the open is caught as well.
/// Returns `None` for anything that fails a check or cannot be opened.
pub fn open_within<P: AsRef<std::path::Path>>(
    path: &std::path::Path,
    roots: impl IntoIterator<Item = P>,
) -> Option<std::fs::File> {
    let roots: Vec<_> = roots
        .into_iter()
        .filter_map(|root| std::fs::canonicalize(root).ok())
        .collect();
    let inside = |real: &std::path::Path| roots.iter().any(|root| real.starts_with(root));
    let real = std::fs::canonicalize(path).ok()?;
    if !real.is_file() || !inside(&real) {
        return None;
    }
    let file = std::fs::File::open(&real).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    // The path can change between the check and the open: ask where the open file really is.
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        if std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
            .is_ok_and(|opened| !inside(&opened))
        {
            return None;
        }
    }
    Some(file)
}
/// Whether `id` can name a packaged background: 1 to 32 lowercase letters, digits or `-`, so
/// `splash/{id}.png` stays inside the package directory.
pub fn valid_background(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}
/// An uploaded background's ID, `custom-` and 32 lowercase hex digits, as Astro creates it.
/// Longer than any packaged background ID, so the two never overlap.
pub fn valid_upload(id: &str) -> bool {
    id.strip_prefix("custom-").is_some_and(|hex| {
        hex.len() == 32
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
/// Returns an LS2 reply unchanged when it reports `"returnValue": true`.
///
/// # Errors
///
/// Any other reply, including one without `returnValue`, becomes an error that quotes it.
pub fn accepted(reply: serde_json::Value) -> Result<serde_json::Value> {
    if reply
        .get("returnValue")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        Ok(reply)
    } else {
        Err(format!("TV service refused: {reply}"))
    }
}

#[cfg(test)]
mod tests;
