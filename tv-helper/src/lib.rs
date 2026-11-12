//! Privileged, bounded TV operations. Shared with Astro for its already-root Home process.
//! No SDL, image decoding, browser engine, or persistent daemon is needed by the helper.
pub mod files;
pub mod home;
pub mod input;
pub mod system;
pub mod uninstall;

pub type Result<T> = std::result::Result<T, String>;
pub const APP: &str = "com.danielcanoh22.astro";
pub const ROOTS: [&str; 5] = [
    "/media/developer/apps/usr/palm/applications",
    "/media/cryptofs/apps/usr/palm/applications",
    "/usr/palm/applications",
    "/media/system/apps/usr/palm/applications",
    "/mnt/otncabi/usr/palm/applications",
];
pub fn valid_app_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .split(['.', '_', '-'])
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric()))
}
/// Opens `path` only if, with every symlink resolved, it is a regular file inside one of
/// `roots`. A path is otherwise only checked as text, so a link inside an application's
/// directory could name any file on the TV, or a FIFO or device that stalls a reader.
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
