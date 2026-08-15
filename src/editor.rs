//! Editor awareness. Primary source: state files written by the "Thoth for
//! VS Code" extension to ~/.thoth/ide/ (active file, selection with line
//! numbers, diagnostics). Fallback: window-title sniffing of known editors,
//! which can only see the active file name.

use serde::Deserialize;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// How long extension state stays valid. The extension heartbeats every 60s.
const STATE_TTL_SECS: u64 = 300;
const MAX_NOTE_DIAGNOSTICS: usize = 15;
/// The note goes into every single request, so it is budgeted like anything
/// else that does. A selection can be a whole file, and one diagnostic can be
/// a page of a type error.
const MAX_SELECTED_CHARS: usize = 2_000;
const MAX_DIAGNOSTIC_CHARS: usize = 200;

pub struct EditorContext {
    /// Full context injected into the model prompt.
    pub note: String,
    /// Short human-readable label for the UI; stable across small changes.
    pub label: String,
}

pub async fn detect() -> Option<EditorContext> {
    if let Some(ctx) = ide_state_context() {
        return Some(ctx);
    }
    title_hack().await
}

#[derive(Deserialize)]
struct IdeState {
    ide: Option<String>,
    workspace: Option<String>,
    updated: Option<u64>,
    #[serde(rename = "activeFile")]
    active_file: Option<String>,
    selection: Option<Selection>,
    #[serde(rename = "selectedText")]
    selected_text: Option<String>,
    #[serde(default)]
    diagnostics: Vec<Diag>,
}

#[derive(Deserialize)]
struct Selection {
    #[serde(rename = "startLine")]
    start_line: u64,
    #[serde(rename = "endLine")]
    end_line: u64,
}

#[derive(Deserialize)]
struct Diag {
    file: String,
    line: u64,
    severity: String,
    message: String,
}

fn state_dir() -> Option<PathBuf> {
    // with no home directory there is no bridge. thoth_home() would fall back
    // to the working directory, and the project's own .thoth is a different
    // thing entirely: never read editor state out of it
    dirs::home_dir()?;
    Some(crate::config::thoth_home().join("ide"))
}

/// Short live label for the TUI, e.g. "In Cargo.toml, 7 lines selected".
/// True when the companion extension is writing state we can read. The
/// `problems` tool and the prompt rules about it are only worth their tokens
/// when there is an editor on the other end.
pub fn connected() -> bool {
    freshest_state().is_some()
}

pub fn live_status() -> Option<String> {
    let state = freshest_state()?;
    status_from_state(&state)
}

fn status_from_state(s: &IdeState) -> Option<String> {
    let f = s.active_file.as_ref()?;
    let base = f.rsplit('/').next().unwrap_or(f);
    let mut out = format!("In {base}");
    if let Some(sel) = &s.selection {
        let n = sel.end_line.saturating_sub(sel.start_line) + 1;
        if n <= 1 {
            out.push_str(&format!(", line {} selected", sel.start_line));
        } else {
            out.push_str(&format!(", {n} lines selected"));
        }
    }
    Some(out)
}

fn ide_state_context() -> Option<EditorContext> {
    let state = freshest_state()?;
    Some(context_from_state(&state))
}

/// Picks the freshest live state whose workspace overlaps our cwd.
fn freshest_state() -> Option<IdeState> {
    let dir = state_dir()?;
    let cwd = std::env::current_dir().ok()?;
    let cwd_s = norm_path(&cwd.to_string_lossy());
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    let mut best: Option<(u64, IdeState)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(state) = serde_json::from_str::<IdeState>(&text) else {
            continue;
        };
        let updated = state.updated.unwrap_or(0);
        if now.saturating_sub(updated) > STATE_TTL_SECS {
            continue;
        }
        let Some(ws) = &state.workspace else { continue };
        let ws_s = norm_path(ws);
        if !(path_contains(&ws_s, &cwd_s) || path_contains(&cwd_s, &ws_s)) {
            continue;
        }
        if best.as_ref().map(|(u, _)| updated > *u).unwrap_or(true) {
            best = Some((updated, state));
        }
    }
    let (_, state) = best?;
    Some(state)
}

/// Normalizes a path for comparison. Windows paths are case-insensitive and
/// may mix separators; Unix paths are case-sensitive and use '/'.
fn norm_path(s: &str) -> String {
    norm_path_impl(s, cfg!(windows))
}

fn norm_path_impl(s: &str, windows: bool) -> String {
    let mut s = if windows {
        s.to_lowercase().replace('/', "\\")
    } else {
        s.to_string()
    };
    let sep = if windows { '\\' } else { '/' };
    while s.len() > 1 && s.ends_with(sep) {
        s.pop();
    }
    s
}

/// True when `inner` equals `outer` or lies inside it, respecting
/// path-component boundaries ("/a/bc" is NOT inside "/a/b").
fn path_contains(outer: &str, inner: &str) -> bool {
    path_contains_impl(outer, inner, cfg!(windows))
}

fn path_contains_impl(outer: &str, inner: &str, windows: bool) -> bool {
    let sep = if windows { '\\' } else { '/' };
    inner == outer || inner.starts_with(&format!("{outer}{sep}"))
}

/// On-demand diagnostics report for the `problems` tool.
pub fn diagnostics_report() -> String {
    let Some(state) = freshest_state() else {
        return "No editor connected: the Thoth VS Code extension is not running for this \
                project. Use the shell (build/typecheck) to find problems instead."
            .into();
    };
    if state.diagnostics.is_empty() {
        return "The editor reports no errors or warnings right now. Note: diagnostics can lag \
                a couple of seconds behind file changes, and files never opened in the editor \
                may not be analyzed. A build/typecheck is the authoritative check."
            .into();
    }
    let mut out = format!(
        "{} problem(s) reported by the editor:\n",
        state.diagnostics.len()
    );
    for d in &state.diagnostics {
        out.push_str(&format!(
            "{}:{} {}: {}\n",
            d.file, d.line, d.severity, d.message
        ));
    }
    out.push_str("(diagnostics may lag 1-2s behind recent edits)");
    out
}

/// Text with a ceiling, saying so when it hits one.
fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!(
        "{head}\n... (cut, {} characters in total)",
        s.chars().count()
    )
}

fn context_from_state(s: &IdeState) -> EditorContext {
    let ide = s.ide.as_deref().unwrap_or("the editor");
    let mut note = format!("The user is working in {ide}.");
    let mut label_parts: Vec<String> = Vec::new();

    if let Some(f) = &s.active_file {
        match &s.selection {
            Some(sel) => {
                note.push_str(&format!(
                    "\nActive file: {f} (lines {}-{} selected)",
                    sel.start_line, sel.end_line
                ));
                label_parts.push(format!("{f} ({}-{})", sel.start_line, sel.end_line));
            }
            None => {
                note.push_str(&format!("\nActive file: {f}"));
                label_parts.push(f.clone());
            }
        }
    }
    if let Some(t) = &s.selected_text
        && !t.trim().is_empty()
    {
        note.push_str(&format!(
            "\nSelected text:\n```\n{}\n```",
            cut(t, MAX_SELECTED_CHARS)
        ));
    }
    if !s.diagnostics.is_empty() {
        let total = s.diagnostics.len();
        note.push_str(&format!("\nCurrent problems in the project ({total}):"));
        for d in s.diagnostics.iter().take(MAX_NOTE_DIAGNOSTICS) {
            note.push_str(&format!(
                "\n- {}:{} {}: {}",
                d.file,
                d.line,
                d.severity,
                cut(&d.message, MAX_DIAGNOSTIC_CHARS)
            ));
        }
        if total > MAX_NOTE_DIAGNOSTICS {
            note.push_str(&format!("\n- ... +{} more", total - MAX_NOTE_DIAGNOSTICS));
        }
        label_parts.push(match total {
            1 => "1 problem".to_string(),
            n => format!("{n} problems"),
        });
    }
    let label = if label_parts.is_empty() {
        ide.to_string()
    } else {
        label_parts.join(", ")
    };
    EditorContext { note, label }
}

// ---- fallback: window-title sniffing (no extension installed) ----

#[cfg(windows)]
async fn title_hack() -> Option<EditorContext> {
    use std::process::Stdio;
    let script = "Get-Process code,Code,cursor,windsurf,notepad++,sublime_text,zed,idea64,\
rustrover64,clion64,pycharm64,webstorm64,goland64,devenv -ErrorAction SilentlyContinue \
| ForEach-Object { $_.MainWindowTitle } | Where-Object { $_ }";
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        tokio::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    let titles = String::from_utf8_lossy(&out.stdout);
    for title in titles.lines() {
        if let Some(name) = parse_title(title.trim())
            && let Some(path) = find_in_project(&name)
        {
            return Some(EditorContext {
                note: format!("The user currently has {path} open in their editor."),
                label: path,
            });
        }
    }
    None
}

#[cfg(not(windows))]
async fn title_hack() -> Option<EditorContext> {
    None
}

/// Extracts a plausible file name from an editor window title, e.g.
/// "● config.rs - thoth - Visual Studio Code" -> "config.rs".
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_title(title: &str) -> Option<String> {
    let title = title.trim_start_matches(['●', '*', '•']).trim();
    let first = ["  -  ", " - ", " — ", " – "]
        .iter()
        .filter_map(|sep| title.split(sep).next())
        .min_by_key(|s| s.len())
        .unwrap_or(title)
        .trim();
    // full path (Notepad++ style) -> keep just the file name
    let name = first.rsplit(['\\', '/']).next().unwrap_or(first).trim();
    let ok = !name.is_empty()
        && name.len() < 120
        && name.contains('.')
        && !name.ends_with('.')
        && !name.contains(' ');
    if ok { Some(name.to_string()) } else { None }
}

/// Finds a file with this name inside the working directory.
#[cfg_attr(not(windows), allow(dead_code))]
fn find_in_project(name: &str) -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let mut checked = 0usize;
    for entry in crate::tools::fs::walk(&cwd) {
        checked += 1;
        if checked > 20_000 {
            break;
        }
        if entry.file_name().to_string_lossy() == name {
            return Some(crate::tools::fs::rel_slash_path(entry.path(), &cwd));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_vscode_titles() {
        assert_eq!(
            parse_title("● config.rs - thoth - Visual Studio Code").as_deref(),
            Some("config.rs")
        );
        assert_eq!(
            parse_title("main.ts - proj - Cursor").as_deref(),
            Some("main.ts")
        );
    }

    #[test]
    fn parses_full_paths() {
        assert_eq!(
            parse_title("*C:\\path\\to\\notes.txt - Notepad++").as_deref(),
            Some("notes.txt")
        );
    }

    #[test]
    fn rejects_non_files() {
        assert_eq!(parse_title("Welcome - Visual Studio Code"), None);
        assert_eq!(parse_title(""), None);
    }

    #[test]
    fn path_matching_windows() {
        let n = |s: &str| norm_path_impl(s, true);
        // case-insensitive, mixed separators, trailing sep
        assert_eq!(n("C:/Users/U/Proj/"), "c:\\users\\u\\proj");
        assert!(path_contains_impl(
            &n("C:\\a\\thoth"),
            &n("c:\\A\\thoth\\src"),
            true
        ));
        // component boundary: thoth-for-vcscode is not inside thoth
        assert!(!path_contains_impl(
            &n("C:\\a\\thoth"),
            &n("C:\\a\\thoth-for-vcscode"),
            true
        ));
    }

    #[test]
    fn path_matching_unix() {
        let n = |s: &str| norm_path_impl(s, false);
        assert_eq!(n("/home/u/proj/"), "/home/u/proj");
        assert!(path_contains_impl(
            &n("/home/u/proj"),
            &n("/home/u/proj/src"),
            false
        ));
        assert!(!path_contains_impl(
            &n("/home/u/proj"),
            &n("/home/u/proj2"),
            false
        ));
        // unix paths are case-sensitive
        assert!(!path_contains_impl(
            &n("/home/u/proj"),
            &n("/home/U/proj/src"),
            false
        ));
    }

    #[test]
    fn builds_live_status() {
        let s: IdeState = serde_json::from_str(
            r#"{"activeFile": "src/main.rs", "selection": {"startLine": 17, "endLine": 23}}"#,
        )
        .unwrap();
        assert_eq!(
            status_from_state(&s).as_deref(),
            Some("In main.rs, 7 lines selected")
        );

        let s: IdeState =
            serde_json::from_str(r#"{"activeFile": "Cargo.toml", "selection": null}"#).unwrap();
        assert_eq!(status_from_state(&s).as_deref(), Some("In Cargo.toml"));

        let s: IdeState = serde_json::from_str(
            r#"{"activeFile": "a.ts", "selection": {"startLine": 4, "endLine": 4}}"#,
        )
        .unwrap();
        assert_eq!(
            status_from_state(&s).as_deref(),
            Some("In a.ts, line 4 selected")
        );
    }

    #[test]
    #[ignore] // reads the real ~/.thoth/ide state — run manually
    fn live_status_real() {
        println!("live_status = {:?}", live_status());
    }

    #[test]
    fn builds_note_from_state() {
        let state: IdeState = serde_json::from_str(
            r#"{
                "ide": "VS Code",
                "workspace": "C:\\proj",
                "updated": 1,
                "activeFile": "src/main.rs",
                "selection": {"startLine": 5, "endLine": 10},
                "selectedText": "fn main() {}",
                "diagnostics": [
                    {"file": "src/lib.rs", "line": 3, "severity": "error", "message": "mismatched types"}
                ]
            }"#,
        )
        .unwrap();
        let ctx = context_from_state(&state);
        assert!(ctx.note.contains("src/main.rs (lines 5-10 selected)"));
        assert!(ctx.note.contains("fn main() {}"));
        assert!(ctx.note.contains("src/lib.rs:3 error: mismatched types"));
        assert_eq!(ctx.label, "src/main.rs (5-10), 1 problem");
    }

    /// The note rides along on every request, so a selected file and a
    /// page-long type error cannot be allowed to take the window with them.
    #[test]
    fn a_huge_selection_is_cut_before_it_reaches_the_prompt() {
        let big = "x".repeat(50_000);
        let state = IdeState {
            ide: None,
            workspace: None,
            updated: None,
            active_file: Some("a.rs".into()),
            selection: None,
            selected_text: Some(big),
            diagnostics: vec![Diag {
                file: "a.rs".into(),
                line: 1,
                severity: "error".into(),
                message: "y".repeat(5_000),
            }],
        };
        let note = context_from_state(&state).note;
        assert!(note.len() < 4_000, "note is {} chars", note.len());
        assert!(note.contains("50000 characters in total"), "{note}");
    }
}
