//! App removal through `com.webos.appInstallService/remove`, confirmed by its completion status.
use crate::{
    Result,
    system::{self, Pipe},
};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
/// Whether the helper may uninstall `id`; see [`crate::removable`].
pub fn allowed(id: &str) -> bool {
    crate::removable(id)
}
/// Classifies one subscription reply from `appInstallService/remove` for app `id`.
///
/// Returns `Ok(true)` once the reply confirms removal, `Ok(false)` while it is still progress.
/// An accepting reply is not completion: only status 31 for the same `id` (and, when present,
/// the same package ID) confirms removal.
///
/// # Errors
///
/// A refused reply, status 25 or an error code fails with the installer's text; a status 31
/// for another app fails as an identity mismatch.
pub fn completed(reply: &Value, id: &str) -> Result<bool> {
    let details = &reply["details"];
    if reply["returnValue"] == false
        || reply["statusValue"] == 25
        || details.get("errorCode").is_some()
    {
        return Err(reply["errorText"]
            .as_str()
            .or(details["reason"].as_str())
            .unwrap_or("App removal failed")
            .into());
    }
    if reply["statusValue"] == 31 {
        if reply["id"] != id
            || (details["packageId"]
                .as_str()
                .is_some_and(|package| !package.is_empty() && package != id))
        {
            return Err("App removal identity mismatch".into());
        }
        return Ok(true);
    }
    Ok(false)
}
/// Incremental parser for the JSON replies `luna-send -i` prints, which arrive split and
/// concatenated arbitrarily across pipe reads.
///
/// Bounded to 64 KiB of combined output and 64 replies, so a chatty or hostile service cannot
/// grow it without limit.
#[derive(Default)]
pub struct Stream {
    buffer: Vec<u8>,
    /// Every reply parsed so far, returned to the caller for diagnostics.
    pub replies: Vec<Value>,
    total: usize,
}
impl Stream {
    /// Consumes one chunk of output and returns `Ok(true)` once a reply confirms the removal of
    /// `id` (see [`completed`]).
    ///
    /// stderr chunks only count towards the output limit. Bytes before a `{` are skipped, and
    /// an incomplete reply waits for the next chunk.
    ///
    /// # Errors
    ///
    /// Fails when a limit is exceeded, a reply is not valid JSON, or a reply reports failure.
    pub fn feed(&mut self, bytes: &[u8], stderr: bool, id: &str) -> Result<bool> {
        self.total += bytes.len();
        if self.total > 64 * 1024 {
            return Err("App removal output limit exceeded".into());
        }
        if stderr {
            return Ok(false);
        }
        self.buffer.extend(bytes);
        loop {
            let Some(start) = self.buffer.iter().position(|b| *b == b'{') else {
                self.buffer.clear();
                return Ok(false);
            };
            if start > 0 {
                self.buffer.drain(..start);
            }
            let mut values =
                serde_json::Deserializer::from_slice(&self.buffer).into_iter::<Value>();
            let reply = match values.next() {
                Some(Ok(reply)) => reply,
                Some(Err(e)) if e.is_eof() => return Ok(false),
                Some(Err(e)) => return Err(format!("Invalid app removal reply: {e}")),
                None => return Ok(false),
            };
            let consumed = values.byte_offset();
            if self.replies.len() >= 64 {
                return Err("Too many app removal replies".into());
            }
            let outcome = completed(&reply, id);
            self.replies.push(reply);
            outcome?;
            if self
                .replies
                .last()
                .is_some_and(|reply| reply["statusValue"] == 31)
            {
                return Ok(true);
            }
            self.buffer.drain(..consumed);
        }
    }
}
/// Uninstalls app `id` and waits up to 15 seconds for the installer to confirm it.
///
/// Never fails: the reply always carries `returnValue`, `errorText` and every installer reply
/// received, so Astro can show why a removal was refused. Protected apps (see [`allowed`]) and
/// callers whose `uid` is not root are refused before any service call. The `luna-send` child is
/// killed and reaped on every path.
pub fn remove(id: &str, uid: u32) -> Value {
    let mut stream = Stream::default();
    let outcome = (|| {
        if !allowed(id) {
            return Err("Protected or invalid application identifier".into());
        }
        if uid != 0 {
            return Err("Homebrew Channel needs root access".into());
        }
        let args = vec![
            "-i".into(),
            "-w".into(),
            "15000".into(),
            "luna://com.webos.appInstallService/remove".into(),
            json!({"id":id,"subscribe":true}).to_string(),
        ];
        let (mut child, rx, threads) = system::spawn("/usr/bin/luna-send", &args)?;
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut closed = 0;
        let result = (|| {
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err("App removal completion timed out".into());
                }
                match rx
                    .recv_timeout(remaining)
                    .map_err(|_| "App removal completion timed out")?
                {
                    Pipe::Out(bytes) => {
                        if stream.feed(&bytes, false, id)? {
                            return Ok(());
                        }
                    }
                    Pipe::Err(bytes) => {
                        stream.feed(&bytes, true, id)?;
                    }
                    Pipe::Closed => {
                        closed += 1;
                        if closed == 2 {
                            return Err("App removal ended without completion".into());
                        }
                    }
                    Pipe::Failure(error) => return Err(error),
                }
            }
        })();
        drop(rx);
        system::reap(&mut child, threads);
        result
    })();
    json!({"returnValue":outcome.is_ok(),"errorText":outcome.err(),"id":id,"replies":stream.replies})
}
