use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::process::Stdio;
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

    let child = cmd.spawn().context("failed to spawn shell")?;
    let out = tokio::select! {
        // dropping the future kills the child (kill_on_drop)
        _ = cancel.cancelled() => bail!("command cancelled by user"),
        res = tokio::time::timeout(timeout, child.wait_with_output()) => {
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
