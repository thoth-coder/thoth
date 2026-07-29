//! Input-line features shared by the TUI and one-shot mode: `@path` file
//! attachments and path completion for them.

/// Size cap for a file pulled in with @path.
const MAX_ATTACH_CHARS: usize = 8_000;

pub fn cwd() -> std::path::PathBuf {
    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
}

/// Pulls `@path` mentions in the user's line into the message as file
/// contents. Returns the text to append and one status line per file.
pub fn expand_mentions(text: &str, base: &std::path::Path) -> (String, Vec<String>) {
    let mut attached = String::new();
    let mut labels: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for word in text.split_whitespace() {
        let Some(token) = word.strip_prefix('@') else {
            continue;
        };
        let token = token.trim_end_matches([',', '.', ';', ':', ')', ']', '?', '!', '"', '\'']);
        if token.is_empty() || !seen.insert(token.to_string()) {
            continue;
        }
        let path = base.join(token);
        if !path.is_file() {
            labels.push(format!("@{token}: no such file, sent as plain text"));
            continue;
        }
        match std::fs::metadata(&path) {
            Ok(m) if m.len() > crate::tools::fs::MAX_FILE_BYTES => {
                labels.push(format!("@{token}: too large to attach, skipped"));
                continue;
            }
            _ => {}
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            labels.push(format!("@{token}: not a text file, skipped"));
            continue;
        };
        let total = content.lines().count();
        let full = content.chars().count() <= MAX_ATTACH_CHARS;
        let body: String = if full {
            content
        } else {
            content.chars().take(MAX_ATTACH_CHARS).collect()
        };
        // the label is data inside a tag the model reads: keep it from
        // closing the tag or forging attributes
        let label = token.replace(['"', '<', '>'], "");
        attached.push_str(&format!(
            "\n\n<file path=\"{label}\"{}>\n{body}\n</file>",
            if full { "" } else { " truncated=\"true\"" }
        ));
        // A whole file pasted in by the user counts as read, so the model can
        // edit it without a round trip. Only inside the project: attaching
        // something from elsewhere must not hand out write rights to it.
        if inside(base, &path) {
            crate::tools::fs::mark_attached(&path, total, full);
        }
        labels.push(if full {
            format!("attached {token} ({total} lines)")
        } else {
            format!("attached {token} (first {MAX_ATTACH_CHARS} chars of {total} lines)")
        });
    }
    (attached, labels)
}

/// True when `path` really lives under `base` (both canonicalized, so `..`
/// and symlinks cannot sneak out).
fn inside(base: &std::path::Path, path: &std::path::Path) -> bool {
    match (base.canonicalize(), path.canonicalize()) {
        (Ok(b), Ok(p)) => p.starts_with(b),
        _ => false,
    }
}

/// "src/too" -> ("src/", "too"); "too" -> ("", "too").
pub fn split_path_fragment(typed: &str) -> (&str, &str) {
    match typed.rfind(['/', '\\']) {
        Some(i) => typed.split_at(i + 1),
        None => ("", typed),
    }
}

/// Entries of `dir` starting with `frag`, directories marked with a trailing
/// slash. Hidden entries only show when the fragment starts with a dot.
pub fn complete_candidates(dir: &std::path::Path, frag: &str) -> Vec<String> {
    let lower = frag.to_lowercase();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') && !frag.starts_with('.') {
            continue;
        }
        // Windows paths are case-insensitive, so match that way everywhere
        if !name.to_lowercase().starts_with(&lower) {
            continue;
        }
        if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            out.push(format!("{name}/"));
        } else {
            out.push(name);
        }
    }
    out.sort_unstable();
    out
}

pub fn common_prefix(items: &[String]) -> String {
    let Some(first) = items.first() else {
        return String::new();
    };
    let mut len = first.chars().count();
    for other in &items[1..] {
        len = len.min(
            first
                .chars()
                .zip(other.chars())
                .take_while(|(a, b)| a == b)
                .count(),
        );
    }
    first.chars().take(len).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("thoth-tui-tests").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("src/model.rs"), "// model\n").unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\n").unwrap();
        std::fs::write(dir.join(".hidden"), "x\n").unwrap();
        dir
    }

    #[test]
    fn attaches_mentioned_files() {
        let base = fixture("attach");
        let (attached, labels) = expand_mentions("look at @src/main.rs, please", &base);
        assert!(attached.contains("<file path=\"src/main.rs\">"));
        assert!(attached.contains("fn main() {}"));
        assert_eq!(labels, vec!["attached src/main.rs (1 lines)"]);
    }

    #[test]
    fn reports_missing_mentions_and_skips_duplicates() {
        let base = fixture("missing");
        let (attached, labels) = expand_mentions("@nope.rs @src/main.rs @src/main.rs", &base);
        assert_eq!(labels.len(), 2);
        assert!(labels[0].contains("no such file"));
        assert_eq!(attached.matches("<file").count(), 1);
    }

    #[test]
    fn completes_paths() {
        let base = fixture("complete");
        assert_eq!(split_path_fragment("src/ma"), ("src/", "ma"));
        assert_eq!(split_path_fragment("Car"), ("", "Car"));
        // one match completes fully
        assert_eq!(complete_candidates(&base.join("src"), "mai"), ["main.rs"]);
        // several matches stop at the shared prefix
        let both = complete_candidates(&base.join("src"), "m");
        assert_eq!(both, ["main.rs", "model.rs"]);
        assert_eq!(common_prefix(&both), "m");
        // directories are marked, hidden files stay hidden until asked for
        assert_eq!(complete_candidates(&base, "s"), ["src/"]);
        assert!(
            complete_candidates(&base, "")
                .iter()
                .all(|c| c != ".hidden")
        );
        assert_eq!(complete_candidates(&base, ".h"), [".hidden"]);
    }
}
