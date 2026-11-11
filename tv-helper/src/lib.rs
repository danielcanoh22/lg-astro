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
/// Whether `path`, with every symlink resolved, is a regular file inside one of `roots`.
/// A path is otherwise only checked as text, so a link inside an application's directory
/// could name any file on the TV, or a FIFO or device that stalls or misleads a reader.
pub fn resolves_within<P: AsRef<std::path::Path>>(
    path: &std::path::Path,
    roots: impl IntoIterator<Item = P>,
) -> bool {
    let Ok(real) = std::fs::canonicalize(path) else {
        return false;
    };
    real.is_file()
        && roots
            .into_iter()
            .any(|root| std::fs::canonicalize(root).is_ok_and(|root| real.starts_with(root)))
}
pub fn valid_background(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
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
