//! External-tool runner with timeouts and process-tree kill (plan P0.6, defect D-03).
//!
//! Every external tool (yt-dlp, ffmpeg, ffprobe, bmxtranswrap) goes through
//! [`run`]. Three properties matter for a daemon that feeds live playout:
//!
//! * **Timeouts.** A hung yt-dlp against a dead CDN otherwise holds a worker
//!   permit forever; with `max_concurrent_jobs=2` two such hangs stop ingest
//!   for the whole newsroom with no error anywhere.
//! * **Process-tree kill.** Killing yt-dlp does not kill the ffmpeg it spawned
//!   to mux fragments. On Windows the only reliable way to take the whole tree
//!   down is a Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: when our
//!   handle to the job closes — including when this process is itself killed —
//!   the kernel terminates every process in it.
//! * **A stderr tail on failure.** Error classification (plan P1.9) reads the
//!   last few KB of stderr; the MCR operator sees it in the job drawer.
//!
//! No console window is ever created: the service has no desktop, and under
//! `run` mode a flashing window per job is unacceptable on an MCR workstation.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// Maximum bytes of stderr retained for error classification and the MCR drawer.
pub const DEFAULT_STDERR_TAIL_BYTES: usize = 8192;

/// What to do with the child's stdout.
pub enum StdoutMode {
    /// Discard it (ffprobe callers that only need the exit code).
    Ignore,
    /// Accumulate the whole stream — for tools whose stdout is the result
    /// (ffprobe JSON, `yt-dlp --dump-json`).
    Capture,
    /// Deliver line by line, for progress parsing. The callback must not block.
    Lines(Box<dyn FnMut(&str) + Send>),
}

/// Runner options. Construct with [`RunOpts::new`] and adjust.
pub struct RunOpts {
    /// Hard wall-clock limit. On expiry the whole process tree is killed and
    /// [`RunOutcome::timed_out`] is set.
    pub timeout: Duration,
    pub stdout: StdoutMode,
    /// How many trailing bytes of stderr to keep.
    pub stderr_tail_bytes: usize,
    /// Kill children of the child too. Always wanted for yt-dlp and ffmpeg.
    pub kill_tree: bool,
    /// Cooperative cancellation (MCR "Cancel" button, lease lost to the reaper).
    pub cancel: Option<CancellationToken>,
    /// Working directory for the child.
    pub current_dir: Option<std::path::PathBuf>,
}

impl RunOpts {
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            stdout: StdoutMode::Ignore,
            stderr_tail_bytes: DEFAULT_STDERR_TAIL_BYTES,
            kill_tree: true,
            cancel: None,
            current_dir: None,
        }
    }

    pub fn capture_stdout(mut self) -> Self {
        self.stdout = StdoutMode::Capture;
        self
    }

    pub fn on_stdout_line<F: FnMut(&str) + Send + 'static>(mut self, f: F) -> Self {
        self.stdout = StdoutMode::Lines(Box::new(f));
        self
    }

    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    pub fn with_current_dir<P: AsRef<Path>>(mut self, dir: P) -> Self {
        self.current_dir = Some(dir.as_ref().to_path_buf());
        self
    }
}

/// Result of a completed, timed-out or cancelled run.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub exit_code: Option<i32>,
    pub success: bool,
    pub elapsed: Duration,
    /// Everything captured when [`StdoutMode::Capture`] was used, else empty.
    pub stdout: String,
    /// Last `stderr_tail_bytes` of stderr, lossily decoded.
    pub stderr_tail: String,
    pub timed_out: bool,
    pub cancelled: bool,
}

impl RunOutcome {
    /// Turn a non-zero exit into an error carrying the stderr tail, so callers
    /// can `?` without losing the reason the tool failed.
    pub fn ok_or_err(self, tool: &str) -> Result<RunOutcome> {
        if self.success {
            return Ok(self);
        }
        if self.timed_out {
            return Err(anyhow!(
                "{tool} timed out after {:.0}s; process tree killed. stderr tail: {}",
                self.elapsed.as_secs_f64(),
                trim_for_message(&self.stderr_tail)
            ));
        }
        if self.cancelled {
            return Err(anyhow!("{tool} was cancelled"));
        }
        Err(anyhow!(
            "{tool} exited with code {:?}. stderr tail: {}",
            self.exit_code,
            trim_for_message(&self.stderr_tail)
        ))
    }
}

fn trim_for_message(s: &str) -> String {
    let t = s.trim();
    if t.is_empty() {
        "(empty)".to_string()
    } else if t.len() > 1200 {
        format!("…{}", &t[t.len() - 1200..])
    } else {
        t.to_string()
    }
}

/// Spawn `program` with `args` and wait for it under the given options.
///
/// Returns `Err` only when the process could not be spawned at all; a tool that
/// ran and failed comes back as a `RunOutcome` with `success == false`, because
/// callers classify those into error codes (plan P1.9).
pub async fn run<S, I>(program: &Path, args: I, opts: RunOpts) -> Result<RunOutcome>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let RunOpts {
        timeout,
        mut stdout,
        stderr_tail_bytes,
        kill_tree,
        cancel,
        current_dir,
    } = opts;

    let started = Instant::now();

    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    // Without this, a dropped future leaves the tool running and holding the
    // temp directory open, so cleanup fails and the next attempt collides.
    cmd.kill_on_drop(true);
    if let Some(dir) = &current_dir {
        cmd.current_dir(dir);
    }

    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        // Deliberately *not* CREATE_BREAKAWAY_FROM_JOB: when our own process is
        // already inside a job without JOB_OBJECT_LIMIT_BREAKAWAY_OK (a CI
        // agent, a terminal that groups its children, the SCM in some
        // configurations) the flag makes CreateProcess fail outright with
        // ERROR_ACCESS_DENIED. Windows 8 and later nest job objects, so a plain
        // spawn can still be assigned to our kill-on-close job below.
        let _ = kill_tree;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("Failed to spawn {:?}", program))?;

    // Assign to a kill-on-close job object before the child does any real work.
    #[cfg(windows)]
    let _job = if kill_tree {
        child.id().and_then(|pid| win_job::JobHandle::assign(pid).ok())
    } else {
        None
    };

    let child_stdout = child.stdout.take();
    let child_stderr = child.stderr.take();

    // Drain stderr into a bounded tail. Draining matters on its own: a tool
    // whose pipe buffer fills blocks forever, which looks exactly like a hang.
    let stderr_task = tokio::spawn(async move {
        let mut tail: Vec<u8> = Vec::with_capacity(stderr_tail_bytes.min(16384));
        if let Some(err) = child_stderr {
            let mut reader = BufReader::new(err);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        tail.extend_from_slice(line.as_bytes());
                        if tail.len() > stderr_tail_bytes {
                            let cut = tail.len() - stderr_tail_bytes;
                            tail.drain(..cut);
                        }
                    }
                }
            }
        }
        String::from_utf8_lossy(&tail).into_owned()
    });

    let stdout_task = tokio::spawn(async move {
        let mut captured = String::new();
        if let Some(out) = child_stdout {
            let mut reader = BufReader::new(out);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => match &mut stdout {
                        StdoutMode::Ignore => {}
                        StdoutMode::Capture => captured.push_str(&line),
                        StdoutMode::Lines(cb) => cb(line.trim_end_matches(['\r', '\n'])),
                    },
                }
            }
        }
        captured
    });

    let mut timed_out = false;
    let mut cancelled = false;

    let status = {
        let wait = child.wait();
        tokio::pin!(wait);
        let sleep = tokio::time::sleep(timeout);
        tokio::pin!(sleep);

        loop {
            let cancelled_fut = async {
                match &cancel {
                    Some(token) => token.cancelled().await,
                    // Never resolves, so `select!` ignores this branch.
                    None => std::future::pending::<()>().await,
                }
            };

            tokio::select! {
                res = &mut wait => break res.ok(),
                _ = &mut sleep => { timed_out = true; break None; }
                _ = cancelled_fut => { cancelled = true; break None; }
            }
        }
    };

    if timed_out || cancelled {
        // Dropping the job handle kills the whole tree; `start_kill` covers the
        // non-Windows path and the case where the job could not be created.
        let _ = child.start_kill();
        #[cfg(windows)]
        drop(_job);
        // Give the OS a moment so `wait` reaps the zombie rather than leaving it.
        let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    }

    // The child's pipes are inherited by anything it spawned, so `read_line`
    // only sees EOF once the *whole tree* is gone. If the job object could not
    // be created (or `kill_tree` was off), an orphaned grandchild would keep
    // these tasks — and therefore this stage, and its worker permit — blocked
    // for as long as it lives, which is precisely the hang the timeout exists
    // to prevent. Bound the drain and give up on the tail instead.
    let drain_budget = if timed_out || cancelled {
        Duration::from_secs(2)
    } else {
        Duration::from_secs(10)
    };
    let stdout_text = join_bounded(stdout_task, drain_budget).await;
    let stderr_tail = join_bounded(stderr_task, drain_budget).await;

    let exit_code = status.as_ref().and_then(|s| s.code());
    let success = status.as_ref().map(|s| s.success()).unwrap_or(false) && !timed_out && !cancelled;

    Ok(RunOutcome {
        exit_code,
        success,
        elapsed: started.elapsed(),
        stdout: stdout_text,
        stderr_tail,
        timed_out,
        cancelled,
    })
}

/// Await a pipe-drain task, abandoning it if an orphan is holding the pipe open.
async fn join_bounded(
    task: tokio::task::JoinHandle<String>,
    budget: Duration,
) -> String {
    match tokio::time::timeout(budget, task).await {
        Ok(Ok(text)) => text,
        // Task panicked, or the drain outlived its budget. Losing the stderr
        // tail is acceptable; blocking the pipeline is not.
        Ok(Err(_)) => String::new(),
        Err(_) => String::new(),
    }
}

/// Convenience wrapper for short tools whose stdout is the answer (ffprobe).
pub async fn run_capture<S, I>(program: &Path, args: I, timeout: Duration) -> Result<RunOutcome>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    run(program, args, RunOpts::new(timeout).capture_stdout()).await
}

#[cfg(windows)]
mod win_job {
    //! Minimal Job Object wrapper. Assigning the child to a job whose limit is
    //! `KILL_ON_JOB_CLOSE` means the kernel terminates the child *and every
    //! process it spawns* when our handle drops — including if `omni-ingest`
    //! itself is killed with `taskkill /F`, which is exactly how an operator
    //! stops a stuck daemon.

    use anyhow::{anyhow, Result};
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JobObjectExtendedLimitInformation,
    };
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};

    pub struct JobHandle(HANDLE);

    impl JobHandle {
        pub fn assign(pid: u32) -> Result<Self> {
            unsafe {
                let job = CreateJobObjectW(None, None)?;
                let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )?;

                let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid)
                    .map_err(|e| anyhow!("OpenProcess({pid}) failed: {e}"))?;
                let assigned = AssignProcessToJobObject(job, process);
                let _ = CloseHandle(process);
                if let Err(e) = assigned {
                    let _ = CloseHandle(job);
                    return Err(anyhow!("AssignProcessToJobObject failed: {e}"));
                }
                Ok(JobHandle(job))
            }
        }
    }

    impl Drop for JobHandle {
        fn drop(&mut self) {
            unsafe {
                // Closing the last handle terminates every process in the job.
                let _ = CloseHandle(self.0);
            }
        }
    }

    // The handle is only ever closed on drop; sharing it across the await points
    // of a single run is sound.
    unsafe impl Send for JobHandle {}
    unsafe impl Sync for JobHandle {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell() -> &'static Path {
        // `cmd` on Windows, `sh` elsewhere; the tests below only use constructs
        // available in both via distinct argument sets.
        if cfg!(windows) {
            Path::new("cmd")
        } else {
            Path::new("sh")
        }
    }

    fn echo_args(text: &str) -> Vec<String> {
        if cfg!(windows) {
            vec!["/c".into(), format!("echo {text}")]
        } else {
            vec!["-c".into(), format!("echo {text}")]
        }
    }

    fn sleep_args(secs: u32) -> Vec<String> {
        if cfg!(windows) {
            // `ping -n N+1 127.0.0.1` sleeps ~N seconds without needing timeout.exe,
            // which refuses to run with redirected stdin.
            vec!["/c".into(), format!("ping -n {} 127.0.0.1 > nul", secs + 1)]
        } else {
            vec!["-c".into(), format!("sleep {secs}")]
        }
    }

    #[tokio::test]
    async fn captures_stdout_and_reports_success() {
        let out = run(
            shell(),
            echo_args("hello-omni"),
            RunOpts::new(Duration::from_secs(30)).capture_stdout(),
        )
        .await
        .unwrap();
        assert!(out.success, "stderr: {}", out.stderr_tail);
        assert!(out.stdout.contains("hello-omni"), "stdout was {:?}", out.stdout);
        assert!(!out.timed_out);
    }

    #[tokio::test]
    async fn non_zero_exit_is_not_success_and_keeps_stderr() {
        let args: Vec<String> = if cfg!(windows) {
            vec!["/c".into(), "echo boom 1>&2 & exit /b 3".into()]
        } else {
            vec!["-c".into(), "echo boom >&2; exit 3".into()]
        };
        let out = run(shell(), args, RunOpts::new(Duration::from_secs(30)))
            .await
            .unwrap();
        assert!(!out.success);
        assert_eq!(out.exit_code, Some(3));
        assert!(out.stderr_tail.contains("boom"), "tail was {:?}", out.stderr_tail);
        // The error message must carry the tail so the MCR drawer can show it.
        let err = out.ok_or_err("testtool").unwrap_err().to_string();
        assert!(err.contains("boom"), "error was {err}");
    }

    #[tokio::test]
    async fn slow_process_is_killed_at_the_timeout() {
        let started = Instant::now();
        let out = run(
            shell(),
            sleep_args(30),
            RunOpts::new(Duration::from_millis(1500)),
        )
        .await
        .unwrap();
        assert!(out.timed_out, "expected a timeout, got {out:?}");
        assert!(!out.success);
        // Must return promptly rather than waiting out the full 30 s.
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "runner blocked for {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn cancellation_token_stops_the_run() {
        let token = CancellationToken::new();
        let t = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            t.cancel();
        });
        let out = run(
            shell(),
            sleep_args(30),
            RunOpts::new(Duration::from_secs(60)).with_cancel(token),
        )
        .await
        .unwrap();
        assert!(out.cancelled, "expected cancellation, got {out:?}");
        assert!(!out.success);
        assert!(!out.timed_out);
    }

    #[tokio::test]
    async fn stdout_lines_callback_sees_every_line() {
        use std::sync::{Arc, Mutex};
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let args: Vec<String> = if cfg!(windows) {
            vec!["/c".into(), "echo one& echo two& echo three".into()]
        } else {
            vec!["-c".into(), "printf 'one\\ntwo\\nthree\\n'".into()]
        };
        let out = run(
            shell(),
            args,
            RunOpts::new(Duration::from_secs(30)).on_stdout_line(move |l| {
                sink.lock().unwrap().push(l.trim().to_string());
            }),
        )
        .await
        .unwrap();
        assert!(out.success);
        let lines = seen.lock().unwrap().clone();
        assert_eq!(lines, vec!["one", "two", "three"], "got {lines:?}");
    }

    #[tokio::test]
    async fn missing_program_is_an_error_not_a_panic() {
        let err = run(
            Path::new("omni-definitely-not-a-real-tool"),
            Vec::<String>::new(),
            RunOpts::new(Duration::from_secs(5)),
        )
        .await;
        assert!(err.is_err());
    }
}
