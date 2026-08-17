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

/// Byte offset of a char index, for editing a string by cursor position.
pub fn byte_idx(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

/// The `@path` word the cursor sits in, as (index of the `@`, text typed
/// after it). `None` when the cursor is not inside a mention. A bare `@`
/// counts, so the picker opens as soon as it is typed.
pub fn mention_at(chars: &[char], cursor: usize) -> Option<(usize, String)> {
    let cur = cursor.min(chars.len());
    let mut start = cur;
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    if start >= cur || chars[start] != '@' {
        return None;
    }
    Some((start, chars[start + 1..cur].iter().collect()))
}

/// The slash command being typed, as (index of the `/`, what follows it).
/// Only at the very start of the message, and only while it is still one
/// word: a `/` further in is a path, and a space means the command is
/// settled and an argument is being written.
pub fn command_at(chars: &[char], cursor: usize) -> Option<(usize, String)> {
    if chars.first() != Some(&'/') {
        return None;
    }
    let cur = cursor.min(chars.len());
    if cur == 0 {
        return None;
    }
    let typed: Vec<char> = chars[1..cur].to_vec();
    if typed.iter().any(|c| c.is_whitespace()) {
        return None;
    }
    Some((0, typed.into_iter().collect()))
}

/// Commands whose name starts with `frag`, with what each one does. Matched
/// case-insensitively, and an empty fragment lists every one of them: a bare
/// `/` is the question "what is there".
pub fn complete_commands(frag: &str) -> Vec<(String, String)> {
    let lower = frag.to_lowercase();
    COMMANDS
        .iter()
        .filter(|(name, _)| name.starts_with(&lower))
        .map(|(name, note)| ((*name).to_string(), (*note).to_string()))
        .collect()
}

/// The slash commands, in the order `/help` lists them, with what each one
/// does in a few words. A test holds this against the help text, so a command
/// added to one and not the other fails the build rather than going missing
/// from completion.
pub const COMMANDS: [(&str, &str); 17] = [
    ("help", "show this help"),
    ("clear", "clear the screen and the conversation"),
    ("compact", "summarize the conversation to free context"),
    ("recap", "load the previous session's summary"),
    ("memory", "show project memory"),
    ("config", "edit the config profiles"),
    ("undo", "put back the files the last request changed"),
    ("copy", "copy the last reply to the clipboard"),
    ("allow", "tools always allowed here"),
    ("mode", "how much thoth asks before acting"),
    ("plan", "short for /mode plan"),
    ("status", "profile, model, api, tokens, cost, uptime"),
    ("init", "analyze the project and write THOTH.md"),
    ("model", "switch model"),
    ("models", "list the models on the server"),
    ("quit", "exit"),
    ("cfg", "short for /config"),
];

/// "src/too" -> ("src/", "too"); "too" -> ("", "too").
pub fn split_path_fragment(typed: &str) -> (&str, &str) {
    match typed.rfind(['/', '\\']) {
        Some(i) => typed.split_at(i + 1),
        None => ("", typed),
    }
}

/// Entries of `dir` starting with `frag`, directories marked with a trailing
/// slash and listed first. Hidden entries only show when the fragment starts
/// with a dot.
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
    // directories first: picking one narrows the list, so it is the move the
    // user is most likely making
    out.sort_unstable_by(|a, b| {
        let file = |s: &String| !s.ends_with('/');
        file(a)
            .cmp(&file(b))
            .then_with(|| a.to_lowercase().cmp(&b.to_lowercase()))
    });
    out
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

    /// Completion offers what `/help` documents, or it offers a command the
    /// user cannot run and hides one they can.
    #[test]
    fn the_command_list_matches_the_help_text() {
        let documented: Vec<String> = crate::ui::HELP
            .lines()
            .filter_map(|l| l.trim_start().strip_prefix('/'))
            .map(|l| {
                l.chars()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .collect::<String>()
            })
            .filter(|n| !n.is_empty())
            .collect();
        for name in &documented {
            assert!(
                COMMANDS.iter().any(|(c, _)| c == name),
                "/{name} is in the help and not in completion"
            );
        }
        for (name, note) in COMMANDS {
            // the two aliases are documented inside another command's line
            if ["cfg", "plan"].contains(&name) {
                continue;
            }
            assert!(
                documented.iter().any(|d| d == name),
                "/{name} completes and is not in the help"
            );
            assert!(!note.is_empty(), "/{name} has nothing to say for itself");
        }
    }

    #[test]
    fn completes_slash_commands() {
        let at = |s: &str, cur: usize| {
            let chars: Vec<char> = s.chars().collect();
            command_at(&chars, cur)
        };
        assert_eq!(at("/mod", 4), Some((0, "mod".into())));
        // a bare slash is a question about what there is
        assert_eq!(at("/", 1), Some((0, String::new())));
        // once there is an argument the command is settled
        assert_eq!(at("/model qwen", 11), None);
        assert_eq!(at("/model ", 7), None);
        // and a slash that is not the first thing typed is a path
        assert_eq!(at("look at src/main.rs", 15), None);
        assert_eq!(at("", 0), None);

        let names: Vec<String> = complete_commands("mod")
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(names, ["mode", "model", "models"]);
        assert_eq!(complete_commands("MODEL").len(), 2, "case does not matter");
        assert_eq!(complete_commands("").len(), COMMANDS.len());
        assert!(complete_commands("zzz").is_empty());
    }

    #[test]
    fn finds_the_mention_under_the_cursor() {
        let chars: Vec<char> = "look at @src/ma now".chars().collect();
        assert_eq!(mention_at(&chars, 15), Some((8, "src/ma".into())));
        // a bare @ opens the picker on the whole directory
        assert_eq!(mention_at(&chars, 9), Some((8, String::new())));
        // outside the mention, and on a word that is not one
        assert_eq!(mention_at(&chars, 4), None);
        assert_eq!(mention_at(&chars, 17), None);
        // right after the completed word, not inside it any more
        let done: Vec<char> = "@src/main.rs ".chars().collect();
        assert_eq!(mention_at(&done, 13), None);
    }

    #[test]
    fn completes_paths() {
        let base = fixture("complete");
        assert_eq!(split_path_fragment("src/ma"), ("src/", "ma"));
        assert_eq!(split_path_fragment("Car"), ("", "Car"));
        // one match completes fully
        assert_eq!(complete_candidates(&base.join("src"), "mai"), ["main.rs"]);
        assert_eq!(
            complete_candidates(&base.join("src"), "m"),
            ["main.rs", "model.rs"]
        );
        // directories are marked and sort ahead of files
        assert_eq!(complete_candidates(&base, "s"), ["src/"]);
        assert_eq!(complete_candidates(&base, "")[0], "src/");
        assert!(
            complete_candidates(&base, "")
                .iter()
                .all(|c| c != ".hidden")
        );
        assert_eq!(complete_candidates(&base, ".h"), [".hidden"]);
    }
}
