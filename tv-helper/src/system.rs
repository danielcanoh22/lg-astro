//! System access for the helper: bounded child processes, LS2 calls, file reads and atomic writes.
//!
//! Everything goes through the [`System`] trait, implemented by [`Tv`] on the device and by a
//! fixture in the tests that maps absolute paths below a temporary root.
use crate::Result;
use serde_json::Value;
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

/// Captured output of a child process that exited within its limits.
pub struct Output {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Whether the child exited with status 0.
    pub success: bool,
}
/// Interval of [`System::pause_briefly`].
pub const BRIEF_PAUSE: Duration = Duration::from_millis(200);
/// The operating system as the helper sees it; tests substitute a fixture.
pub trait System {
    /// Maps an absolute TV path to the path to use: unchanged on the TV, below the fixture root
    /// in tests.
    fn path(&self, path: &Path) -> PathBuf;
    /// Runs `program` with `args` and no stdin, failing once it takes longer than `timeout` or
    /// stdout and stderr together exceed `limit` bytes.
    fn run(
        &self,
        program: &str,
        args: &[String],
        timeout: Duration,
        limit: usize,
    ) -> Result<Output>;
    /// Waits one second between attempts at a TV service that is still catching up.
    fn pause(&self) {
        thread::sleep(Duration::from_secs(1));
    }
    /// For waits that usually end within a fraction of a second, such as a terminated process.
    fn pause_briefly(&self) {
        thread::sleep(BRIEF_PAUSE);
    }
    /// Sends `SIGTERM` to `pid`. A process that has already exited counts as terminated.
    fn terminate(&self, pid: u32) -> Result<()>;
    /// Returns the effective user ID.
    fn uid(&self) -> u32;
}
/// The real TV.
pub struct Tv;
// Declared here because `libc` is only a dependency of webOS builds.
#[cfg(unix)]
unsafe extern "C" {
    fn geteuid() -> u32;
    fn kill(pid: i32, signal: i32) -> i32;
}
impl System for Tv {
    fn path(&self, path: &Path) -> PathBuf {
        path.into()
    }
    fn run(
        &self,
        program: &str,
        args: &[String],
        timeout: Duration,
        limit: usize,
    ) -> Result<Output> {
        run_bounded(program, args, timeout, limit)
    }
    fn terminate(&self, pid: u32) -> Result<()> {
        #[cfg(unix)]
        {
            // SAFETY: `kill` takes plain integers and has no memory-safety preconditions. Only
            // PIDs chosen by Home's process matcher (2..=i32::MAX) reach this point, so the
            // cast cannot turn into a process-group or broadcast signal. ESRCH (3): already gone.
            if unsafe { kill(pid as i32, 15) } == 0
                || std::io::Error::last_os_error().raw_os_error() == Some(3)
            {
                return Ok(());
            }
            Err(std::io::Error::last_os_error().to_string())
        }
        #[cfg(not(unix))]
        {
            let _ = pid;
            Err("TV signals require Unix".into())
        }
    }
    fn uid(&self) -> u32 {
        #[cfg(unix)]
        {
            // SAFETY: `geteuid` takes no arguments, always succeeds and touches no memory.
            unsafe { geteuid() }
        }
        #[cfg(not(unix))]
        {
            u32::MAX
        }
    }
}
/// Fails unless the effective user is root.
///
/// # Errors
///
/// Returns "Run the TV helper as root" for any other user.
pub fn root(system: &impl System) -> Result<()> {
    if system.uid() == 0 {
        Ok(())
    } else {
        Err("Run the TV helper as root".into())
    }
}

/// One event from a child's output readers (see [`spawn`]).
pub(crate) enum Pipe {
    /// A chunk of stdout.
    Out(Vec<u8>),
    /// A chunk of stderr.
    Err(Vec<u8>),
    /// One of the two pipes reached end of file; two of these mean the child closed both.
    Closed,
    /// Reading a pipe failed.
    Failure(String),
}
/// Starts `program` with stdin closed and one reader thread per output pipe.
///
/// Each pipe is read independently, so a child filling stderr cannot deadlock a stdout reader.
/// The caller's event loop bounds the combined output and deadline from the returned channel,
/// then calls [`reap`] on every completion path. The channel is bounded: readers wait while the
/// caller falls behind and stop once it drops the receiver.
pub(crate) fn spawn(
    program: &str,
    args: &[String],
) -> Result<(Child, mpsc::Receiver<Pipe>, Vec<thread::JoinHandle<()>>)> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{program}: {e}"))?;
    let (tx, rx) = mpsc::sync_channel(8);
    let mut threads = vec![];
    for (pipe, stderr) in [
        (
            child
                .stdout
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
            false,
        ),
        (
            child
                .stderr
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
            true,
        ),
    ] {
        let tx = tx.clone();
        threads.push(thread::spawn(move || {
            if let Some(mut pipe) = pipe {
                let mut buffer = [0; 4096];
                loop {
                    match pipe.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(n) => {
                            if tx
                                .send(if stderr {
                                    Pipe::Err(buffer[..n].to_vec())
                                } else {
                                    Pipe::Out(buffer[..n].to_vec())
                                })
                                .is_err()
                            {
                                return;
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(e) => {
                            let _ = tx.send(Pipe::Failure(e.to_string()));
                            break;
                        }
                    }
                }
            }
            let _ = tx.send(Pipe::Closed);
        }));
    }
    Ok((child, rx, threads))
}
/// Kills the child if it still runs, waits for it and joins its reader threads.
///
/// Drop the receiver first: a reader blocked on a full channel only returns once it is gone.
pub(crate) fn reap(child: &mut Child, threads: Vec<thread::JoinHandle<()>>) {
    let _ = child.kill();
    let _ = child.wait();
    for t in threads {
        let _ = t.join();
    }
}
/// [`System::run`] for the real TV.
///
/// # Errors
///
/// Fails when the program cannot start, a pipe cannot be read, `timeout` passes or the combined
/// output exceeds `limit`. The child is killed and reaped in every case.
pub fn run_bounded(
    program: &str,
    args: &[String],
    timeout: Duration,
    limit: usize,
) -> Result<Output> {
    let (mut child, rx, threads) = spawn(program, args)?;
    let deadline = Instant::now() + timeout;
    let mut stdout = vec![];
    let mut stderr = vec![];
    let mut closed = 0;
    let result = (|| {
        while closed < 2 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!("{program}: timed out"));
            }
            match rx
                .recv_timeout(remaining)
                .map_err(|_| format!("{program}: timed out"))?
            {
                Pipe::Out(bytes) => stdout.extend(bytes),
                Pipe::Err(bytes) => stderr.extend(bytes),
                Pipe::Closed => closed += 1,
                Pipe::Failure(e) => return Err(e),
            }
            if stdout.len() + stderr.len() > limit {
                return Err(format!("{program}: output limit exceeded"));
            }
        }
        loop {
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                return Ok(Output {
                    stdout,
                    stderr,
                    success: status.success(),
                });
            }
            if Instant::now() >= deadline {
                return Err(format!("{program}: timed out"));
            }
            thread::sleep(Duration::from_millis(5));
        }
    })();
    drop(rx);
    reap(&mut child, threads);
    result
}
/// Runs a system command with an 8 s timeout and a 2 MiB output cap.
///
/// # Errors
///
/// Besides the [`System::run`] failures, a non-zero exit fails with the start of its stderr.
pub fn command(system: &impl System, program: &str, args: &[&str]) -> Result<()> {
    let result = system.run(
        program,
        &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        Duration::from_secs(8),
        2 * 1024 * 1024,
    )?;
    if result.success {
        Ok(())
    } else {
        Err(format!(
            "{program}: {}",
            String::from_utf8_lossy(&result.stderr)
                .chars()
                .take(480)
                .collect::<String>()
        ))
    }
}
/// Calls `luna://{service}/{method}` once with `luna-send -n 1` and returns its JSON reply.
///
/// The reply is returned whether or not it reports success; pass it to [`crate::accepted`] when
/// only success is acceptable.
///
/// # Errors
///
/// Fails when `luna-send` fails or times out, or when its output is not a JSON object with a
/// boolean `returnValue`.
pub fn luna(system: &impl System, service: &str, method: &str, payload: &Value) -> Result<Value> {
    let uri = format!("luna://{service}/{method}");
    let result = system.run(
        "/usr/bin/luna-send",
        &[
            "-n".into(),
            "1".into(),
            "-w".into(),
            "4000".into(),
            uri.clone(),
            payload.to_string(),
        ],
        Duration::from_secs(8),
        2 * 1024 * 1024,
    )?;
    if !result.success {
        return Err(format!(
            "{uri}: {}",
            String::from_utf8_lossy(&result.stderr)
                .chars()
                .take(480)
                .collect::<String>()
        ));
    }
    let reply: Value =
        serde_json::from_slice(&result.stdout).map_err(|e| format!("{uri}: invalid JSON: {e}"))?;
    if !reply.is_object() || !reply["returnValue"].is_boolean() {
        return Err(format!("{uri}: invalid reply"));
    }
    Ok(reply)
}
/// Reads a regular file of at most `limit` bytes; see [`read_open`].
///
/// The path is opened as given, following symlinks, so use it only for paths the helper or
/// root controls, not for paths a caller chose.
pub fn read(system: &impl System, path: impl AsRef<Path>, limit: usize) -> Result<Vec<u8>> {
    let file = fs::File::open(system.path(path.as_ref()))
        .map_err(|e| format!("{}: {e}", path.as_ref().display()))?;
    read_open(file, limit)
}
/// Reads an already opened regular file of at most `limit` bytes.
///
/// The size is checked before reading and enforced while reading, so a file that grows in
/// between is refused rather than read past `limit`.
///
/// # Errors
///
/// Fails for anything other than a regular file, a file over `limit`, or a read error.
pub fn read_open(file: fs::File, limit: usize) -> Result<Vec<u8>> {
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.len() > limit as u64 {
        return Err("File size/type limit exceeded".into());
    }
    let mut bytes = vec![];
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > limit {
        return Err("File size limit exceeded".into());
    }
    Ok(bytes)
}
/// Reads and parses a JSON file of at most 1 MiB.
pub fn json(system: &impl System, path: impl AsRef<Path>) -> Result<Value> {
    serde_json::from_slice(&read(system, path, 1024 * 1024)?).map_err(|e| e.to_string())
}
/// Whether anything, including a dangling symlink, exists at `path`.
pub fn exists(system: &impl System, path: impl AsRef<Path>) -> bool {
    fs::symlink_metadata(system.path(path.as_ref())).is_ok()
}
/// Atomically replaces `path` with `bytes`, created with permission bits `mode`.
///
/// The data goes to a new hidden file in the same directory (created exclusively, so a planted
/// file or symlink at that name is never written through), is synced, then renamed over the
/// destination, so readers see either the old or the new content, never a partial file. The
/// temporary file is removed when any step fails.
pub fn write(system: &impl System, path: impl AsRef<Path>, bytes: &[u8], mode: u32) -> Result<()> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let destination = system.path(path.as_ref());
    let name = destination
        .file_name()
        .ok_or("Invalid destination")?
        .to_string_lossy();
    let tmp = destination.with_file_name(format!(
        ".{name}.new-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    let mut file = options.open(&tmp).map_err(|e| e.to_string())?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, &destination)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(|e| e.to_string())
}
/// Writes `value` as pretty-printed JSON with mode 0644; see [`write()`].
pub fn write_json(
    system: &impl System,
    path: impl AsRef<Path>,
    value: &impl serde::Serialize,
) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    write(system, path, &bytes, 0o644)
}
/// Removes a file or symlink (never its target); one that is already missing counts as removed.
pub fn remove(system: &impl System, path: impl AsRef<Path>) -> Result<()> {
    match fs::remove_file(system.path(path.as_ref())) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}
/// Whether `a` and `b` resolve to the same inode, as a bind mount does. Always `false` off Unix.
pub fn same_file(system: &impl System, a: impl AsRef<Path>, b: impl AsRef<Path>) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (
            fs::metadata(system.path(a.as_ref())),
            fs::metadata(system.path(b.as_ref())),
        ) {
            (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (system, a, b);
        false
    }
}
/// Creates a directory unless a real one is already there.
///
/// # Errors
///
/// Refuses a symlink or any other entry in its place, so root never follows a planted link.
pub fn mkdir(system: &impl System, path: impl AsRef<Path>) -> Result<()> {
    let path = system.path(path.as_ref());
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_dir() => return Ok(()),
        Ok(_) => return Err("Expected a real directory".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.to_string()),
    }
    fs::create_dir(path).map_err(|e| e.to_string())
}
/// Creates a symlink at `target` that points to `source`; fails if `target` exists.
pub fn link(
    system: &impl System,
    source: impl AsRef<Path>,
    target: impl AsRef<Path>,
) -> Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(system.path(source.as_ref()), system.path(target.as_ref()))
            .map_err(|e| e.to_string())
    }
    #[cfg(not(unix))]
    {
        let _ = (system, source, target);
        Err("TV symlinks require Unix".into())
    }
}
