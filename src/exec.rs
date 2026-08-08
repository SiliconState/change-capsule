//! Running a verification command inside a capsule workspace.
//!
//! This is what separates *executed* evidence from a *claim*. Capsule spawns
//! the program itself, in the capsule workspace, and observes the exit status
//! and output bytes directly. Nothing here trusts the caller for the outcome.
//!
//! No shell is involved. The caller supplies an argument vector, which is
//! passed to the operating system unchanged, so quoting and word splitting
//! never enter the picture.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// Largest combined stdout and stderr capture, in bytes.
///
/// A command that produces more fails closed rather than being silently
/// truncated: a digest over truncated output would not describe the run.
pub(crate) const OUTPUT_CAP: usize = 8 * 1024 * 1024;

/// First gap between exit checks for a timed command.
const INITIAL_POLL_INTERVAL: Duration = Duration::from_millis(1);

/// Longest gap between exit checks, once the backoff has grown into it.
const MAX_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// What Capsule observed when it ran a command.
#[derive(Debug)]
pub(crate) struct Execution {
    /// Exit status, or 128 + signal when a Unix signal terminated the process.
    pub(crate) exit_code: i32,
    /// Domain-separated digest over both captured streams.
    pub(crate) output_sha256: String,
    /// Combined byte length of both streams.
    pub(crate) output_bytes: u64,
    /// Trailing captured text, for the evidence summary.
    pub(crate) tail: String,
}

/// Run `argv` with `workspace` as the working directory.
///
/// The child inherits the environment, because a verification command normally
/// needs `PATH` and a toolchain. It inherits no standard input: an interactive
/// prompt would otherwise hang an unattended harness forever.
pub(crate) fn run(
    workspace: &Path,
    argv: &[String],
    timeout: Option<Duration>,
) -> Result<Execution> {
    let (program, arguments) = argv
        .split_first()
        .ok_or_else(|| Error::InvalidInput("a command to run must not be empty".to_owned()))?;
    let mut child = Command::new(program)
        .args(arguments)
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| Error::Io {
            path: workspace.to_path_buf(),
            source,
        })?;

    let stdout = child.stdout.take().ok_or(Error::CaptureWorker)?;
    let stderr = child.stderr.take().ok_or(Error::CaptureWorker)?;
    let stdout_worker = spawn_reader(stdout);
    let stderr_worker = spawn_reader(stderr);

    let status = match timeout {
        Some(limit) => wait_with_timeout(&mut child, limit)?,
        None => child.wait().map_err(|source| Error::Io {
            path: workspace.to_path_buf(),
            source,
        })?,
    };
    // Both pipes close when the child and any process it left holding them exit,
    // so joining here cannot outlive the wait above by more than that drain.
    let out = stdout_worker.join().map_err(|_| Error::CaptureWorker)??;
    let err = stderr_worker.join().map_err(|_| Error::CaptureWorker)??;

    let total = out.len().saturating_add(err.len());
    if total > OUTPUT_CAP {
        return Err(Error::InvalidInput(format!(
            "verification command produced {total} bytes of output, exceeding the {OUTPUT_CAP}-byte capture bound"
        )));
    }

    let mut digest = Sha256::new();
    digest.update(b"change-capsule evidence output v1\0");
    digest.update((out.len() as u64).to_be_bytes());
    digest.update(&out);
    digest.update((err.len() as u64).to_be_bytes());
    digest.update(&err);

    Ok(Execution {
        exit_code: exit_code(status),
        output_sha256: hex::encode(digest.finalize()),
        output_bytes: total as u64,
        tail: tail_text(&out, &err),
    })
}

fn wait_with_timeout(
    child: &mut std::process::Child,
    limit: Duration,
) -> Result<std::process::ExitStatus> {
    // `Instant + Duration` panics when the sum is unrepresentable, and the limit
    // comes straight from a caller-supplied `--timeout-seconds`. A deadline that
    // far out cannot be reached anyway, so treat it as no deadline at all rather
    // than aborting the process.
    let deadline = Instant::now().checked_add(limit);
    let mut backoff = INITIAL_POLL_INTERVAL;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(source) => {
                return Err(Error::Io {
                    path: std::path::PathBuf::from("verification command"),
                    source,
                });
            }
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            // Only the spawned process is killed, not any process group it
            // created: doing that portably needs `pre_exec`, and this crate
            // forbids unsafe code. Returning here detaches the capture threads,
            // which then exit as soon as the pipes close. A killed command that
            // left a surviving grandchild holding the write end keeps them
            // parked until it exits, so callers that expect that should isolate
            // verification commands at the process level. `docs/security.md`
            // states this limit.
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::InvalidInput(format!(
                "verification command exceeded its {}-second timeout and was killed; no evidence was recorded",
                limit.as_secs()
            )));
        }
        // Poll tightly at first so a fast command is not charged a full interval,
        // then back off so a long test suite costs almost no wakeups.
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(MAX_POLL_INTERVAL);
    }
}

fn spawn_reader<R: Read + Send + 'static>(
    mut source: R,
) -> std::thread::JoinHandle<Result<Vec<u8>>> {
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        // Read one byte past the cap so an over-long stream is detected rather
        // than silently truncated to exactly the bound.
        source
            .by_ref()
            .take(OUTPUT_CAP as u64 + 1)
            .read_to_end(&mut buffer)
            .map_err(|source| Error::Io {
                path: std::path::PathBuf::from("verification command output"),
                source,
            })?;
        // Drain the rest so the child never blocks writing into a full pipe.
        let _ = std::io::copy(&mut source, &mut std::io::sink());
        Ok(buffer)
    })
}

#[cfg(unix)]
fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(-1)
}

#[cfg(not(unix))]
fn exit_code(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(-1)
}

/// Last few kilobytes of output, preferring stderr, as valid UTF-8.
fn tail_text(out: &[u8], err: &[u8]) -> String {
    const TAIL_BYTES: usize = 4 * 1024;
    let source = if err.is_empty() { out } else { err };
    let start = source.len().saturating_sub(TAIL_BYTES);
    String::from_utf8_lossy(&source[start..])
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .collect::<String>()
        .trim()
        .to_owned()
}
