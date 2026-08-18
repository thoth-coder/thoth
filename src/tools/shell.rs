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

/// What to hand the shell so it writes to the pipe and nowhere else.
///
/// Redirecting stdout and stderr is not enough on Windows. A console program
/// inherits its parent's console and can draw on it through the console api,
/// which is how `Write-Progress`, `Invoke-WebRequest`'s progress bar and any
/// coloured `Write-Host` land on top of thoth's interface, and how a child
/// that changes the console mode takes the terminal's escape handling with
/// it. CREATE_NO_WINDOW gives the child no console at all: the pipes still
/// carry everything it means for us, and it has nothing to scribble on.
fn own_console(cmd: &mut tokio::process::Command) {
    #[cfg(windows)]
    {
        // tokio's Command carries creation_flags itself
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}

/// The same for the background spawn, which uses the blocking Command.
fn own_console_std(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}

/// The command as the shell will be given it. On Windows that means one
/// statement in front of it: PowerShell 5.1 writes its output in the console
/// codepage, so anything past ascii arrives as mojibake through a pipe, and
/// `[Console]::OutputEncoding` is the setting that decides it. What the user
/// sees in the UI is still their own command; this is the encoding the pipe
/// is read with, not a change to what runs.
fn shell_argv(command: &str) -> (&'static str, Vec<String>) {
    if cfg!(windows) {
        (
            "powershell",
            vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                format!(
                    "[Console]::OutputEncoding=[Text.Encoding]::UTF8; {}",
                    drop_stderr_merge(command)
                ),
            ],
        )
    } else {
        ("sh", vec!["-c".into(), command.into()])
    }
}

/// Takes `2>&1` out of a command on Windows, where it does the opposite of
/// what whoever wrote it meant.
///
/// PowerShell 5.1 does not merge a native program's stderr into its stdout.
/// It wraps every stderr line in an ErrorRecord, so a build that printed one
/// ordinary progress line comes back as a NativeCommandError carrying the
/// command text, a caret diagram and a FullyQualifiedErrorId, and the real
/// output is buried under PowerShell's own account of it. It also sets the
/// failure flag on a command that succeeded. Both streams are captured and
/// handed over whole and labelled either way, so the redirect buys nothing
/// and costs the output's readability. This is the one thing a model writes
/// out of `sh` habit that quietly corrupts what it gets back, rather than
/// failing where it can see it.
///
/// Every standalone one goes, not only a trailing one: it is as wrong in the
/// middle of a pipeline as at the end. It has to be its own word, so
/// `echo 12>&1` and `cmd 2>file` are left alone. A literal `2>&1` inside a
/// quoted string would go too, which is worth the exchange: a command that
/// wants to print those four characters is rarer than a build whose output
/// comes back unreadable.
fn drop_stderr_merge(command: &str) -> String {
    const TOKEN: &str = "2>&1";
    if !cfg!(windows) || !command.contains(TOKEN) {
        return command.to_string();
    }
    let mut out = String::with_capacity(command.len());
    let mut rest = command;
    while let Some(i) = rest.find(TOKEN) {
        let after = &rest[i + TOKEN.len()..];
        let standalone = rest[..i].ends_with([' ', '\t'])
            && after
                .chars()
                .next()
                .is_none_or(|c| c.is_whitespace() || c == '|' || c == ';');
        if standalone {
            out.push_str(rest[..i].trim_end());
            // keep one space so `a 2>&1 b` does not become `ab`, but not in
            // front of a separator that never wanted one
            if after
                .chars()
                .next()
                .is_some_and(|c| !c.is_whitespace() && c != ';' && c != '|')
            {
                out.push(' ');
            }
        } else {
            out.push_str(&rest[..i + TOKEN.len()]);
        }
        rest = after;
    }
    out.push_str(rest);
    let trimmed = out.trim_end();
    if trimmed.len() == out.len() {
        out
    } else {
        trimmed.to_string()
    }
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
    let (program, args) = shell_argv(command);
    let mut c = Command::new(program);
    c.args(args);
    own_console_std(&mut c);
    let child = c
        .stdin(StdStdio::null())
        .stdout(out)
        .stderr(err)
        .spawn()
        .context("failed to spawn background process")?;
    let pid = child.id();
    started_here().lock().unwrap().push(pid);
    let (kill_hint, wait_hint) = if cfg!(windows) {
        (format!("taskkill /PID {pid} /T /F"), "Start-Sleep 15")
    } else {
        (format!("kill {pid}"), "sleep 15")
    };
    Ok(format!(
        "Started in background: pid {pid}\nOutput is being written to: {}\n\
         It has produced nothing yet. Let it run before looking: shell `{wait_hint}`, then \
         read_file on that path, and again if it has not finished. Reading it in a tight loop \
         only shows you the same bytes.\nStop the process when done with shell: {kill_hint}",
        log.display()
    ))
}

/// A failure that is the shell rejecting the syntax rather than the program
/// disagreeing with anything, said in a way that names the fix.
///
/// This is one command's shell, not a language: the same sentence written for
/// `sh` is a parse error in Windows PowerShell, and a model that has read far
/// more `sh` than PowerShell writes it that way roughly every time. Left
/// alone it costs a whole turn per occurrence, and the model has to infer the
/// fix from a caret diagram.
fn shell_syntax_hint(stderr: &str) -> Option<&'static str> {
    if !cfg!(windows) {
        return None;
    }
    // PowerShell 5.1 has no pipeline chain operators and no ternary
    if stderr.contains("&&") || stderr.contains("||") {
        return Some(
            "hint: this shell is Windows PowerShell 5.1, where `&&` and `||` are not operators. \
             Run the parts as separate commands, or join them with `;` to run both regardless.",
        );
    }
    None
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
    let (program, args) = shell_argv(&a.command);
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args);
    own_console(&mut cmd);
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
        if let Some(h) = shell_syntax_hint(&stderr) {
            s.push_str(h);
            s.push('\n');
        }
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

    /// PowerShell does not merge a native program's stderr into its stdout
    /// the way `sh` does: it wraps each line in an ErrorRecord, so a build
    /// that printed one ordinary progress line comes back as a
    /// NativeCommandError with a caret diagram around it and the real output
    /// buried underneath. Both streams are captured and labelled anyway.
    #[test]
    fn a_trailing_stderr_merge_comes_off_the_command() {
        let go = |c: &str| drop_stderr_merge(c);
        if !cfg!(windows) {
            // sh merges the streams the way the words say, so nothing to do
            assert_eq!(go("cargo check 2>&1"), "cargo check 2>&1");
            return;
        }
        assert_eq!(go("cargo check 2>&1"), "cargo check");
        assert_eq!(go("cargo check 2>&1   "), "cargo check");
        assert_eq!(go("cargo check	2>&1"), "cargo check");
        // as wrong in the middle of a pipeline as at the end
        assert_eq!(
            go("cargo build 2>&1 | Select-Object -First 5"),
            "cargo build | Select-Object -First 5"
        );
        assert_eq!(
            go("a --version 2>&1; b --version 2>&1"),
            "a --version; b --version"
        );
        // but it has to be its own word
        assert_eq!(go("echo 12>&1"), "echo 12>&1");
        assert_eq!(go("cargo check 2>log"), "cargo check 2>log");
        assert_eq!(go("cargo check"), "cargo check");
    }

    /// A shell rejecting the syntax is not the program disagreeing with
    /// anything, and the caret diagram it prints does not say which shell
    /// this is. Left alone it costs a whole turn every time.
    #[test]
    fn a_posix_ism_on_powershell_is_named_as_one() {
        let hint = shell_syntax_hint("The token '&&' is not a valid statement separator");
        if cfg!(windows) {
            assert!(hint.expect("windows names it").contains(";"));
        } else {
            assert!(hint.is_none(), "sh has these operators, nothing to say");
        }
        assert!(shell_syntax_hint("no such file or directory").is_none());
    }
}
