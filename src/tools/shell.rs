use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 600;

#[derive(Deserialize)]
pub struct ShellArgs {
    pub command: String,
    /// Run detached (servers/watchers): returns immediately with pid + log.
    #[serde(default)]
    pub background: bool,
    pub timeout_secs: Option<u64>,
}

/// Spawns a detached process whose output goes to a log file, so servers and
/// watchers don't block the agent.
fn run_background(command: &str) -> Result<String> {
    use std::process::{Command, Stdio as StdStdio};
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let log = std::env::temp_dir().join(format!("thoth-bg-{ts}.log"));
    let out = std::fs::File::create(&log).context("cannot create log file")?;
    let err = out.try_clone().context("cannot clone log handle")?;
    let mut c = if cfg!(windows) {
        let mut c = Command::new("powershell");
        c.args(["-NoProfile", "-Command", command]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", command]);
        c
    };
    let child = c
        .stdin(StdStdio::null())
        .stdout(out)
        .stderr(err)
        .spawn()
        .context("failed to spawn background process")?;
    let pid = child.id();
    started_here().lock().unwrap().push(pid);
    let kill_hint = if cfg!(windows) {
        format!("taskkill /PID {pid} /T /F")
    } else {
        format!("kill {pid}")
    };
    Ok(format!(
        "Started in background: pid {pid}\nOutput is being written to: {}\nCheck it with read_file on that path (wait a moment first). Stop the process when done with shell: {kill_hint}",
        log.display()
    ))
}

/// Pids thoth put into the background. A model that starts a server and
/// then says "closing it now" without doing it leaves it holding the port
/// for the rest of the day; whether it is still up is not the model's word
/// about itself, it is a question the operating system answers.
fn started_here() -> &'static Mutex<Vec<u32>> {
    static P: OnceLock<Mutex<Vec<u32>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(Vec::new()))
}

/// The ones thoth started that are still running. Checked by signalling
/// them with no signal on unix, and by asking the task list on windows.
pub fn still_running() -> Vec<u32> {
    let pids = started_here().lock().unwrap().clone();
    pids.into_iter().filter(|p| alive(*p)).collect()
}

fn alive(pid: u32) -> bool {
    if cfg!(windows) {
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
            .unwrap_or(false)
    } else {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

/// Everything past this is discarded as it arrives: the model only ever sees
/// the first few thousand characters anyway.
const MAX_CAPTURE_BYTES: usize = 200_000;

/// Drains a pipe to the end (so the child never blocks on a full buffer) but
/// keeps at most MAX_CAPTURE_BYTES of it.
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(pipe: &mut Option<R>) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    let Some(r) = pipe else {
        return Vec::new();
    };
    let mut kept: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if kept.len() < MAX_CAPTURE_BYTES {
                    let room = MAX_CAPTURE_BYTES - kept.len();
                    kept.extend_from_slice(&buf[..n.min(room)]);
                }
            }
        }
    }
    kept
}

pub async fn run(a: ShellArgs, cancel: CancellationToken) -> Result<String> {
    if a.background {
        return run_background(&a.command);
    }
    let timeout = Duration::from_secs(
        a.timeout_secs
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .min(MAX_TIMEOUT_SECS),
    );
    let mut cmd = if cfg!(windows) {
        let mut c = tokio::process::Command::new("powershell");
        c.args(["-NoProfile", "-Command", &a.command]);
        c
    } else {
        let mut c = tokio::process::Command::new("sh");
        c.args(["-c", &a.command]);
        c
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn().context("failed to spawn shell")?;
    // read the pipes with a ceiling: a chatty build or a runaway `yes` would
    // otherwise buffer gigabytes before the output cap ever applies
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let collect = async {
        let (o, e) = tokio::join!(read_capped(&mut stdout_pipe), read_capped(&mut stderr_pipe));
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((o, e, status))
    };
    let (out_bytes, err_bytes, status) = tokio::select! {
        // dropping the future kills the child (kill_on_drop)
        _ = cancel.cancelled() => bail!("command cancelled by user"),
        res = tokio::time::timeout(timeout, collect) => {
            match res {
                Err(_) => bail!(
                    "command timed out after {}s. For servers/watchers use background=true; \
                     for slow builds pass timeout_secs (max {MAX_TIMEOUT_SECS})",
                    timeout.as_secs()
                ),
                Ok(r) => r.context("failed to run command")?,
            }
        }
    };
    let out = std::process::Output {
        status,
        stdout: out_bytes,
        stderr: err_bytes,
    };

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let mut s = String::new();
    if !stdout.trim().is_empty() {
        s.push_str(stdout.trim_end());
        s.push('\n');
    }
    if !stderr.trim().is_empty() {
        s.push_str("--- stderr ---\n");
        s.push_str(stderr.trim_end());
        s.push('\n');
    }
    if !out.status.success() {
        let code = out
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "?".into());
        s.push_str(&format!("(command FAILED, exit code: {code})\n"));
    } else if s.is_empty() {
        s = "(command succeeded, exit code 0, no output)".into();
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reports_exit_code_and_output() {
        let out = run(
            ShellArgs {
                command: "echo hello-shell".into(),
                background: false,
                timeout_secs: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(out.contains("hello-shell"), "{out}");
        assert!(!out.contains("FAILED"), "{out}");
    }

    /// A command that prints forever must not be able to fill memory.
    #[tokio::test]
    async fn caps_captured_output() {
        // ~40 bytes per line, so 8000 lines is well past the 200k cap
        let cmd = if cfg!(windows) {
            "1..8000 | ForEach-Object { 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx' }"
        } else {
            "for i in $(seq 1 8000); do echo xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; done"
        };
        let out = run(
            ShellArgs {
                command: cmd.into(),
                background: false,
                timeout_secs: Some(120),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        // the tool-level cap trims this further; what matters here is that
        // the capture stopped at the ceiling instead of buffering everything
        assert!(
            out.len() <= MAX_CAPTURE_BYTES + 64,
            "captured {} bytes",
            out.len()
        );
    }
}
