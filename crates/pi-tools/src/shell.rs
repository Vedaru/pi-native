//! Bounded shell execution.
//!
//! Both the `bash` tool and the RPC out-of-band command run through this. It
//! keeps memory flat regardless of how much a command prints:
//!
//! * stdout/stderr are read concurrently so a full pipe cannot deadlock;
//! * only a tail of `max_bytes` per stream is kept in memory;
//! * once the combined output grows past a separate spool threshold it is
//!   written to a temp file, so a truncated result can still point at the
//!   full output without duplicating the in-memory tail;
//! * the process is bounded by a wall-clock deadline.
//! * a deadline kills the whole process group, not just the direct child.
//!
//! The returned tail is capped by [`truncate_tail`] exactly like pi's tool.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::truncate::{truncate_tail, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};

/// Default wall-clock limit for a shell command when the caller gives none.
/// pi has no default timeout; a bounded unit must, or a wedged command pins the
/// process forever. Ten minutes is long enough for real builds.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// Bytes of prefix buffered before output is spooled to disk. Kept
/// deliberately larger than [`DEFAULT_MAX_BYTES`] so the spool's in-memory
/// `head` does not merely duplicate the tail we already retain.
const SPOOL_THRESHOLD_BYTES: usize = 256 * 1024;

/// The result of a bounded shell run.
#[derive(Debug, Clone)]
pub struct ShellOutput {
    /// The (possibly tail-truncated) combined stdout+stderr.
    pub output: String,
    /// Process exit code, or `-1` when the process was killed/timed out.
    pub exit_code: i32,
    /// Whether the returned text is shorter than what the command emitted.
    pub truncated: bool,
    /// Whether the deadline elapsed and the command was killed.
    pub timed_out: bool,
    /// Full output on disk when it exceeded the in-memory cap.
    pub spool_path: Option<PathBuf>,
}

/// Run `command` through `sh -c` in `cwd` with a bounded, spooled capture.
pub fn run_shell(command: &str, cwd: &Path, timeout: Option<Duration>) -> ShellOutput {
    run_shell_with(command, cwd, timeout, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES)
}

/// Like [`run_shell`], with explicit byte/line caps (used by tests).
pub fn run_shell_with(
    command: &str,
    cwd: &Path,
    timeout: Option<Duration>,
    max_bytes: usize,
    max_lines: usize,
) -> ShellOutput {
    let mut builder = Command::new("sh");
    builder
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Put the child in its own process group so a timeout can kill its
    // descendants too. `process_group` is stable on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        builder.process_group(0);
    }

    let mut child = match builder.spawn() {
        Ok(child) => child,
        Err(error) => {
            return ShellOutput {
                output: format!("failed to spawn command: {error}"),
                exit_code: -1,
                truncated: false,
                timed_out: false,
                spool_path: None,
            };
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    // Decouple the spool threshold from the tail cap: the tail keeps the last
    // `max_bytes`, while the spool buffers a separate (larger) prefix before
    // hitting disk. Scaling by `max_bytes` keeps small test caps small.
    let spool_threshold = max_bytes.max(SPOOL_THRESHOLD_BYTES);
    let spool = Arc::new(Mutex::new(Spool::new(spool_threshold)));

    let cap = max_bytes;
    let out_spool = spool.clone();
    let out_handle =
        stdout.map(|pipe| std::thread::spawn(move || read_stream(pipe, cap, out_spool)));
    let err_spool = spool.clone();
    let err_handle =
        stderr.map(|pipe| std::thread::spawn(move || read_stream(pipe, cap, err_spool)));

    let deadline = Instant::now() + timeout.unwrap_or(DEFAULT_TIMEOUT);
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    timed_out = true;
                    kill_process_group(&child);
                    let _ = child.kill();
                    break child.wait().ok();
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => {
                let _ = child.kill();
                break child.wait().ok();
            }
        }
    };

    let stdout = out_handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    let stderr = err_handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();

    let mut combined = String::from_utf8_lossy(&stdout.tail).into_owned();
    let err = String::from_utf8_lossy(&stderr.tail);
    if !err.is_empty() {
        combined.push_str(&err);
    }
    let total = stdout.total + stderr.total;
    let capped = truncate_tail(&combined, max_lines, max_bytes);
    let spool_path = spool.lock().ok().and_then(|spool| spool.path.clone());

    let exit_code = status
        .as_ref()
        .and_then(|status| status.code())
        .unwrap_or(-1);

    ShellOutput {
        truncated: capped.truncated || total > capped.content.len(),
        output: capped.content,
        exit_code,
        timed_out,
        spool_path,
    }
}

#[cfg(unix)]
fn kill_process_group(child: &std::process::Child) {
    // A negative pid targets the whole process group (`process_group(0)`).
    let pid = child.id() as i32;
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_process_group(_child: &std::process::Child) {}

#[derive(Default)]
struct Tail {
    tail: Vec<u8>,
    total: usize,
}

fn read_stream(mut pipe: impl Read, cap: usize, spool: Arc<Mutex<Spool>>) -> Tail {
    let mut tail: VecDeque<u8> = VecDeque::with_capacity(cap.min(64 * 1024));
    let mut total = 0usize;
    let mut chunk = [0u8; 32 * 1024];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                total += n;
                if let Ok(mut spool) = spool.lock() {
                    spool.push(&chunk[..n]);
                }
                for &byte in &chunk[..n] {
                    if tail.len() == cap {
                        tail.pop_front();
                    }
                    tail.push_back(byte);
                }
            }
        }
    }
    Tail {
        tail: tail.into_iter().collect(),
        total,
    }
}

/// Lazily spools output to a temp file once it passes `threshold` bytes. The
/// in-memory `head` holds the pre-threshold prefix until the file exists. The
/// file itself is capped so a runaway command cannot fill the disk.
struct Spool {
    file: Option<std::fs::File>,
    path: Option<PathBuf>,
    head: Vec<u8>,
    threshold: usize,
    written: usize,
    limit: usize,
}

impl Spool {
    fn new(threshold: usize) -> Self {
        // At most 64 MB on disk (and never less than the in-memory cap).
        let limit = (64 * 1024 * 1024).max(threshold);
        Self {
            file: None,
            path: None,
            head: Vec::new(),
            threshold,
            written: 0,
            limit,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        if let Some(file) = self.file.as_mut() {
            write_capped(file, &mut self.written, self.limit, chunk);
            return;
        }
        if self.head.len() + chunk.len() <= self.threshold {
            self.head.extend_from_slice(chunk);
            return;
        }
        if let Some(path) = spool_path() {
            if let Ok(mut file) = std::fs::File::create(&path) {
                let head = std::mem::take(&mut self.head);
                write_capped(&mut file, &mut self.written, self.limit, &head);
                write_capped(&mut file, &mut self.written, self.limit, chunk);
                self.file = Some(file);
                self.path = Some(path);
                return;
            }
        }
        // Could not spool; keep a bounded prefix so memory stays flat.
        self.head.extend_from_slice(chunk);
        if self.head.len() > self.threshold {
            let excess = self.head.len() - self.threshold;
            self.head.drain(..excess);
        }
    }
}

fn write_capped(file: &mut std::fs::File, written: &mut usize, limit: usize, chunk: &[u8]) {
    if *written >= limit {
        return;
    }
    let remaining = limit - *written;
    let take = chunk.len().min(remaining);
    if file.write_all(&chunk[..take]).is_ok() {
        *written += take;
    } else {
        *written = limit;
    }
}

fn spool_path() -> Option<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!("pipelets-bash-{}-{n}.log", std::process::id()));
    Some(path)
}
