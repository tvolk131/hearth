//! The one way the helper runs an embedded Python worker.
//!
//! Workers start their external commands (`zfs send/recv`, `dd`, `sgdisk`, ...)
//! in separate sessions and clean them up from a SIGTERM handler. Killing a
//! worker with SIGKILL skips that handler and leaves those commands running
//! unsupervised, so every exit path here (normal completion, timeout, and the
//! caller dropping the future) gives the worker SIGTERM and a grace period
//! before SIGKILL.

use std::{process::Stdio, time::Duration};

use anyhow::{Context as _, Result, bail, ensure};
use serde_json::Value;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
    time::timeout,
};

use super::SAFE_PATH;

/// How long a worker gets to stop its own children after SIGTERM. Workers
/// wait up to 10 seconds per child before escalating themselves.
pub(super) const DEFAULT_GRACE: Duration = Duration::from_secs(30);
/// How much of the worker's stderr is kept in an error message.
const STDERR_TAIL_CHARS: usize = 2500;
/// How much of the worker's stderr is held in memory; older output is dropped.
const STDERR_KEEP_BYTES: usize = 64 * 1024;

pub(super) struct Worker<'a> {
    /// Short name used in error messages, e.g. "shrink adapter".
    pub label: &'a str,
    /// Complete Python source, run with `python3 -I -c`.
    pub script: &'a str,
    /// Extra positional arguments after the script.
    pub args: &'a [&'a str],
    /// JSON written to the worker's stdin, or `None` for an empty stdin.
    pub input: Option<&'a Value>,
    /// Wall-clock budget before the worker is terminated.
    pub budget: Duration,
    /// Time between SIGTERM and SIGKILL.
    pub grace: Duration,
    /// Largest stdout accepted, in bytes.
    pub max_output: usize,
    /// Run with this group ID. Production passes `Some(0)`; tests pass `None`.
    pub gid: Option<u32>,
    /// What a timeout means for the operation, appended to the error.
    pub timeout_hint: &'a str,
}

impl Worker<'_> {
    /// Run the worker and return its stdout. Fails on a non-zero exit (with
    /// the tail of stderr), a timeout, or output over `max_output`.
    pub async fn run(&self) -> Result<Vec<u8>> {
        let mut command = Command::new("python3");
        command
            .arg("-I")
            .arg("-c")
            .arg(self.script)
            .args(self.args)
            .env_clear()
            .env("PATH", SAFE_PATH)
            .env("LC_ALL", "C")
            .stdin(if self.input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(gid) = self.gid {
            command.gid(gid);
        }
        let mut child = Supervised::new(
            command
                .spawn()
                .with_context(|| format!("{} could not start", self.label))?,
            self.grace,
        );
        let process = child.get();
        let writer = match (self.input, process.stdin.take()) {
            (Some(input), Some(mut stdin)) => {
                let bytes = serde_json::to_vec(input)?;
                Some(tokio::spawn(async move {
                    stdin.write_all(&bytes).await?;
                    stdin.shutdown().await
                }))
            }
            _ => None,
        };
        let limit = self.max_output as u64 + 1;
        let mut stdout = process.stdout.take().context("worker stdout missing")?;
        let mut stderr = process.stderr.take().context("worker stderr missing")?;
        let output = tokio::spawn(async move {
            let mut bytes = Vec::new();
            (&mut stdout).take(limit).read_to_end(&mut bytes).await?;
            // Keep draining so a chatty worker never blocks on a full pipe.
            tokio::io::copy(&mut stdout, &mut tokio::io::sink()).await?;
            std::io::Result::Ok(bytes)
        });
        let errors = tokio::spawn(async move {
            // Keep only the most recent output so a flood cannot grow memory.
            let mut kept = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                let n = stderr.read(&mut chunk).await?;
                if n == 0 {
                    break;
                }
                kept.extend_from_slice(&chunk[..n]);
                if kept.len() > 2 * STDERR_KEEP_BYTES {
                    kept.drain(..kept.len() - STDERR_KEEP_BYTES);
                }
            }
            std::io::Result::Ok(kept)
        });

        let status = match timeout(self.budget, child.get().wait()).await {
            Ok(status) => status?,
            Err(_) => {
                if let Some(writer) = &writer {
                    writer.abort();
                }
                child.terminate().await;
                output.abort();
                errors.abort();
                bail!("{} timed out; {}", self.label, self.timeout_hint);
            }
        };
        child.finished();
        if let Some(writer) = writer {
            // A worker may exit (successfully or not) before reading all of
            // its input; the exit status below is what matters.
            let _ = writer.await;
        }
        let bytes = output.await??;
        let detail = errors.await??;
        ensure!(
            status.success(),
            "{} failed: {}",
            self.label,
            stderr_tail(&detail)
        );
        ensure!(
            bytes.len() <= self.max_output,
            "{} output exceeds {} bytes",
            self.label,
            self.max_output
        );
        Ok(bytes)
    }

    /// Run the worker and parse its stdout as JSON.
    pub async fn run_json(&self) -> Result<Value> {
        let bytes = self.run().await?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("{} returned invalid JSON", self.label))
    }
}

fn stderr_tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let skip = text.chars().count().saturating_sub(STDERR_TAIL_CHARS);
    text.chars().skip(skip).collect()
}

/// Owns a running worker and makes sure it is stopped gracefully, even if the
/// future that owns it is dropped.
struct Supervised {
    child: Option<Child>,
    grace: Duration,
    /// SIGTERM already sent. Workers must get it exactly once: a second
    /// signal would interrupt their own cleanup handler.
    signalled: bool,
}

impl Supervised {
    fn new(child: Child, grace: Duration) -> Self {
        Self {
            child: Some(child),
            grace,
            signalled: false,
        }
    }

    fn get(&mut self) -> &mut Child {
        self.child.as_mut().expect("worker already released")
    }

    /// The worker exited on its own; nothing to clean up.
    fn finished(&mut self) {
        self.child = None;
    }

    /// SIGTERM, wait up to the grace period, then SIGKILL, and reap. The
    /// child stays owned by `self` until it is reaped, so a caller cancelled
    /// during the grace period still escalates through `Drop`.
    async fn terminate(&mut self) {
        let grace = self.grace;
        if let Some(child) = self.child.as_mut() {
            if !self.signalled {
                signal_term(child);
                self.signalled = true;
            }
            escalate(child, grace).await;
        }
        self.child = None;
    }
}

impl Drop for Supervised {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        if !self.signalled {
            signal_term(&mut child);
        }
        // Without a runtime to wait on, the worker still has SIGTERM and its
        // parent-death signal; dropping the handle does not kill it.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let grace = self.grace;
            runtime.spawn(async move { escalate(&mut child, grace).await });
        }
    }
}

/// After SIGTERM: wait up to `grace`, then SIGKILL, and reap.
async fn escalate(child: &mut Child, grace: Duration) {
    if timeout(grace, child.wait()).await.is_err() {
        let _ = child.kill().await;
    }
    let _ = child.wait().await;
}

/// Send SIGTERM if the child has not been reaped. An unreaped child keeps its
/// PID, so this cannot signal an unrelated process.
fn signal_term(child: &mut Child) {
    if let (Ok(None), Some(pid)) = (child.try_wait(), child.id())
        && let Ok(pid) = libc::pid_t::try_from(pid)
    {
        // SAFETY: kill(2) with a valid signal number has no memory effects.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Instant;

    fn worker<'a>(script: &'a str, input: Option<&'a Value>, budget: Duration) -> Worker<'a> {
        Worker {
            label: "test worker",
            script,
            args: &[],
            input,
            budget,
            grace: Duration::from_secs(5),
            max_output: 1024,
            gid: None,
            timeout_hint: "inspect state",
        }
    }

    fn alive(pid: i32) -> bool {
        // SAFETY: signal 0 only checks for existence.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    async fn wait_gone(pid: i32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if !alive(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    /// A worker shaped like the real ones: it starts a long-running command
    /// in its own session, prints that command's PID to a file, and stops it
    /// from a SIGTERM handler.
    const SESSION_WORKER: &str = r#"
import json, os, signal, subprocess, sys, time
params = json.loads(sys.stdin.read())
child = subprocess.Popen(["sleep", "60"], start_new_session=True)
def stop(signum, frame):
    os.killpg(child.pid, signal.SIGTERM)
    child.wait()
    sys.exit(1)
signal.signal(signal.SIGTERM, stop)
with open(params["pid_file"], "w") as f:
    f.write(str(child.pid))
time.sleep(60)
"#;

    async fn grandchild_pid(path: &std::path::Path) -> i32 {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(text) = std::fs::read_to_string(path)
                && let Ok(pid) = text.trim().parse()
            {
                return pid;
            }
            assert!(Instant::now() < deadline, "worker never started its child");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn returns_stdout_and_reads_input_from_stdin() {
        let input = json!({"value": 41});
        let out = worker(
            "import json,sys; print(json.dumps({'v': json.loads(sys.stdin.read())['value'] + 1}))",
            Some(&input),
            Duration::from_secs(30),
        )
        .run_json()
        .await
        .unwrap();
        assert_eq!(out, json!({"v": 42}));
    }

    #[tokio::test]
    async fn failure_reports_the_tail_of_stderr() {
        let error = worker(
            "import sys; sys.stderr.write('x' * 5000 + 'END'); sys.exit(3)",
            None,
            Duration::from_secs(30),
        )
        .run()
        .await
        .unwrap_err()
        .to_string();
        assert!(error.starts_with("test worker failed: "));
        assert!(error.ends_with("END"));
        assert!(error.len() < 2600);
    }

    #[tokio::test]
    async fn oversized_output_is_refused() {
        let error = worker("print('y' * 5000)", None, Duration::from_secs(30))
            .run()
            .await
            .unwrap_err();
        assert!(error.to_string().contains("output exceeds 1024 bytes"));
    }

    #[tokio::test]
    async fn timeout_lets_the_worker_stop_its_session_children() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let input = json!({"pid_file": pid_file.clone()});
        let run = tokio::spawn(async move {
            worker(SESSION_WORKER, Some(&input), Duration::from_secs(5))
                .run()
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
        let grandchild = grandchild_pid(&pid_file).await;
        let error = run.await.unwrap().unwrap_err();
        assert!(error.contains("timed out; inspect state"), "{error}");
        assert!(
            wait_gone(grandchild).await,
            "session child {grandchild} survived the worker timeout"
        );
    }

    #[tokio::test]
    async fn dropping_the_future_lets_the_worker_stop_its_session_children() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let input = json!({"pid_file": pid_file.clone()});
        let run = tokio::spawn(async move {
            worker(SESSION_WORKER, Some(&input), Duration::from_secs(300))
                .run()
                .await
                .map(|_| ())
        });
        let grandchild = grandchild_pid(&pid_file).await;
        run.abort();
        let _ = run.await;
        assert!(
            wait_gone(grandchild).await,
            "session child {grandchild} survived cancellation"
        );
    }

    #[tokio::test]
    async fn worker_ignoring_sigterm_is_killed_after_the_grace_period() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let input = json!({"pid_file": pid_file.clone()});
        let run = tokio::spawn(async move {
            let mut w = worker(
                "import json,os,signal,sys,time\n\
                 p = json.loads(sys.stdin.read())\n\
                 signal.signal(signal.SIGTERM, signal.SIG_IGN)\n\
                 open(p['pid_file'], 'w').write(str(os.getpid()))\n\
                 time.sleep(60)\n",
                Some(&input),
                Duration::from_secs(5),
            );
            w.grace = Duration::from_secs(1);
            w.run().await.map(|_| ()).map_err(|e| e.to_string())
        });
        let pid = grandchild_pid(&pid_file).await;
        let error = run.await.unwrap().unwrap_err();
        assert!(error.contains("timed out"), "{error}");
        assert!(!alive(pid), "worker {pid} survived SIGKILL escalation");
    }

    #[tokio::test]
    async fn sigterm_is_sent_exactly_once_on_timeout_and_cancellation() {
        // Counts SIGTERMs, then exits after a pause long enough for a
        // duplicate signal to arrive.
        const COUNTING: &str = "import json,signal,sys,time\n\
             p = json.loads(sys.stdin.read())\n\
             n = [0]\n\
             def h(s, f):\n    n[0] += 1\n    open(p['count'], 'w').write(str(n[0]))\n\
             signal.signal(signal.SIGTERM, h)\n\
             open(p['ready'], 'w').write('1')\n\
             end = time.time() + 60\n\
             while time.time() < end and n[0] == 0: time.sleep(0.05)\n\
             time.sleep(1.5)\n\
             sys.exit(1)\n";
        for cancel in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let count = dir.path().join("count");
            let ready = dir.path().join("ready");
            let input = json!({"count": count.clone(), "ready": ready.clone()});
            let budget = Duration::from_secs(if cancel { 60 } else { 3 });
            let run = tokio::spawn(async move {
                worker(COUNTING, Some(&input), budget)
                    .run()
                    .await
                    .map(|_| ())
            });
            grandchild_pid_or_flag(&ready).await;
            if cancel {
                run.abort();
            }
            let _ = run.await;
            tokio::time::sleep(Duration::from_secs(3)).await;
            assert_eq!(
                std::fs::read_to_string(&count).unwrap(),
                "1",
                "cancel={cancel}: SIGTERM must be delivered exactly once"
            );
        }
    }

    async fn grandchild_pid_or_flag(path: &std::path::Path) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !path.exists() {
            assert!(Instant::now() < deadline, "worker never became ready");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn stderr_flood_is_bounded_and_keeps_the_tail() {
        let error = worker(
            "import sys\nfor _ in range(2000): sys.stderr.write('z' * 1000)\nsys.stderr.write('LAST')\nsys.exit(2)\n",
            None,
            Duration::from_secs(30),
        )
        .run()
        .await
        .unwrap_err()
        .to_string();
        assert!(error.ends_with("LAST"), "{}", &error[error.len() - 20..]);
    }

    /// The real embedded workers read their parameters from stdin. A wrong
    /// helper PID makes each one stop at its first check after parsing.
    async fn embedded_worker_error(script: &str) -> String {
        let input = json!({"helper_pid": 1});
        worker(script, Some(&input), Duration::from_secs(60))
            .run()
            .await
            .unwrap_err()
            .to_string()
    }

    #[tokio::test]
    async fn shrink_worker_reads_parameters_from_stdin() {
        let error = embedded_worker_error(super::super::shrink::WORKER).await;
        assert!(error.contains("unexpected helper parent"), "{error}");
    }

    #[tokio::test]
    async fn grow_worker_reads_parameters_from_stdin() {
        let error = embedded_worker_error(&super::super::grow::grow_script()).await;
        assert!(error.contains("unexpected helper parent"), "{error}");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn migration_worker_reads_parameters_from_stdin() {
        let error = embedded_worker_error(super::super::migration::WORKER).await;
        assert!(
            error.contains("migration helper exited before adapter initialization"),
            "{error}"
        );
    }
}
