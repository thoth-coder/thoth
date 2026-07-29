use anyhow::{Context, Result, bail};
use globset::GlobBuilder;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use walkdir::WalkDir;

/// What the model has actually seen of each file this session, as merged
/// line ranges. edit_file needs any read; overwriting a file with write_file
/// needs every line covered, so neither a 1-line peek nor two peeks at the
/// first and last line can unlock a blind rewrite.
#[derive(Default)]
struct ReadState {
    /// Sorted, non-overlapping (first, last) line ranges, 1-based inclusive.
    seen: Vec<(usize, usize)>,
    total: usize,
}

impl ReadState {
    fn add(&mut self, first: usize, last: usize) {
        if last < first {
            return;
        }
        self.seen.push((first, last));
        self.seen.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(self.seen.len());
        for &(a, b) in &self.seen {
            match merged.last_mut() {
                // ranges touching end to end count as continuous
                Some((_, e)) if a <= *e + 1 => *e = (*e).max(b),
                _ => merged.push((a, b)),
            }
        }
        self.seen = merged;
    }

    fn covers_all(&self) -> bool {
        self.total > 0 && self.seen.first() == Some(&(1, self.total))
    }
}

fn read_registry() -> &'static Mutex<HashMap<PathBuf, ReadState>> {
    static R: OnceLock<Mutex<HashMap<PathBuf, ReadState>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn registry_key(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// Records that lines `first..=last` of a file with `total` lines were shown.
fn mark_read(p: &Path, first: usize, last: usize, total: usize) {
    let mut r = read_registry().lock().unwrap();
    let e = r.entry(registry_key(p)).or_default();
    // the file changed size since the last read: start over
    if e.total != total {
        e.seen.clear();
        e.total = total;
    }
    e.add(first, last);
}

/// A file the user pulled in with `@path`. `full` is false when the
/// attachment was truncated, which must not unlock a blind overwrite.
pub fn mark_attached(p: &Path, lines: usize, full: bool) {
    if full {
        mark_read(p, 1, lines, lines);
    } else {
        // seen from the top but not to the end: enough for edit_file only
        mark_read(p, 1, lines.saturating_sub(1).max(1), lines.max(2));
    }
}

fn was_read(p: &Path) -> bool {
    read_registry()
        .lock()
        .unwrap()
        .get(&registry_key(p))
        .is_some_and(|s| !s.seen.is_empty())
}

fn was_fully_read(p: &Path) -> bool {
    read_registry()
        .lock()
        .unwrap()
        .get(&registry_key(p))
        .is_some_and(ReadState::covers_all)
}

const MAX_READ_LINES: usize = 600;
const MAX_LINE_CHARS: usize = 500;
const MAX_ENTRIES: usize = 500;
/// Refuse to slurp anything bigger than this into the context; the model
/// should use offset/limit or grep instead.
pub const MAX_FILE_BYTES: u64 = 2_000_000;

pub fn resolve(path: &str) -> PathBuf {
    let p = PathBuf::from(path);
    if p.is_absolute() {
        p
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(p)
    }
}

/// True when a tool argument points inside the working directory. Used to
/// decide whether an operation is routine or needs the user to approve it.
pub fn inside_project(path: &str) -> bool {
    let Ok(cwd) = std::env::current_dir() else {
        return false;
    };
    let target = resolve(path);
    match (cwd.canonicalize(), target.canonicalize()) {
        (Ok(c), Ok(t)) => t.starts_with(c),
        // a file that does not exist yet: judge the path we would create
        (Ok(c), Err(_)) => target.starts_with(&c) && !path.contains(".."),
        _ => false,
    }
}

/// Directories that are never worth walking into.
pub fn skip_dir(name: &str) -> bool {
    (name.starts_with('.') && name.len() > 1)
        || matches!(
            name,
            "node_modules" | "target" | "dist" | "build" | "venv" | "__pycache__" | "vendor"
        )
}

#[derive(Deserialize)]
pub struct ReadArgs {
    pub path: String,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

pub fn read_file(a: ReadArgs) -> Result<String> {
    let path = resolve(&a.path);
    if let Ok(meta) = std::fs::metadata(&path)
        && meta.len() > MAX_FILE_BYTES
    {
        bail!(
            "{} is {} MB, too large to read into the context. Use grep to find what you need, \
             or read_file with offset and limit",
            path.display(),
            meta.len() / 1_000_000
        );
    }
    let bytes = std::fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    let offset = a.offset.unwrap_or(1).max(1);
    let limit = a.limit.unwrap_or(MAX_READ_LINES).min(MAX_READ_LINES);

    let mut out = String::new();
    let mut shown = 0usize;
    let mut total = 0usize;
    let mut last_shown = 0usize;
    for (i, line) in text.lines().enumerate() {
        let n = i + 1;
        total = n;
        if n < offset || shown >= limit {
            continue;
        }
        let line = if line.chars().count() > MAX_LINE_CHARS {
            let cut: String = line.chars().take(MAX_LINE_CHARS).collect();
            format!("{cut}…")
        } else {
            line.to_string()
        };
        out.push_str(&format!("{n:>6}\t{line}\n"));
        shown += 1;
        last_shown = n;
    }
    if total == 0 {
        mark_read(&path, 1, 1, 1);
        return Ok("(empty file)".into());
    }
    mark_read(&path, offset.min(total), last_shown, total);
    if offset > 1 || last_shown < total {
        out.push_str(&format!(
            "(showing lines {}-{} of {}, use offset/limit to read more)\n",
            offset.min(total),
            last_shown,
            total
        ));
    }
    Ok(out)
}

#[derive(Deserialize)]
pub struct WriteArgs {
    pub path: String,
    pub content: String,
}

pub fn write_file(a: WriteArgs) -> Result<String> {
    let path = resolve(&a.path);
    if path.exists() && !was_fully_read(&path) {
        bail!(
            "{} already exists and you have not read it completely. Read the whole file first, \
             and for modifying an existing file prefer edit_file over overwriting it",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create directory {}", parent.display()))?;
    }
    std::fs::write(&path, &a.content)
        .with_context(|| format!("cannot write {}", path.display()))?;
    // the model authored the whole file, so it has seen all of it
    let lines = a.content.lines().count().max(1);
    mark_read(&path, 1, lines, lines);
    Ok(format!(
        "Wrote {} lines ({} bytes) to {}",
        a.content.lines().count(),
        a.content.len(),
        path.display()
    ))
}

#[derive(Deserialize)]
pub struct EditArgs {
    pub path: String,
    pub old_string: String,
    pub new_string: String,
    #[serde(default)]
    pub replace_all: bool,
}

pub fn edit_file(a: EditArgs) -> Result<String> {
    let path = resolve(&a.path);
    if !was_read(&path) {
        bail!(
            "you have not read {} in this session. read it with read_file before editing",
            path.display()
        );
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    if a.old_string == a.new_string {
        bail!("old_string and new_string are identical");
    }
    let count = content.matches(&a.old_string).count();
    if count == 0 {
        bail!(
            "old_string not found in {}. read the file and copy the exact text",
            path.display()
        );
    }
    if !a.replace_all && count > 1 {
        bail!(
            "old_string appears {count} times in {}. add surrounding context to make it unique, or set replace_all",
            path.display()
        );
    }
    let new_content = if a.replace_all {
        content.replace(&a.old_string, &a.new_string)
    } else {
        content.replacen(&a.old_string, &a.new_string, 1)
    };
    std::fs::write(&path, new_content)
        .with_context(|| format!("cannot write {}", path.display()))?;
    let n = if a.replace_all { count } else { 1 };
    Ok(format!("Edited {} ({n} replacement(s))", path.display()))
}

#[derive(Deserialize)]
pub struct ListArgs {
    pub path: Option<String>,
}

pub fn list_dir(a: ListArgs) -> Result<String> {
    let path = resolve(a.path.as_deref().unwrap_or("."));
    let mut dirs: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&path)
        .with_context(|| format!("cannot list {}", path.display()))?
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            dirs.push(format!("{name}/"));
        } else {
            files.push(name);
        }
    }
    dirs.sort();
    files.sort();
    let all: Vec<String> = dirs.into_iter().chain(files).collect();
    if all.is_empty() {
        return Ok("(empty directory)".into());
    }
    let total = all.len();
    let mut out = all
        .into_iter()
        .take(MAX_ENTRIES)
        .collect::<Vec<_>>()
        .join("\n");
    if total > MAX_ENTRIES {
        out.push_str(&format!("\n… ({} more entries)", total - MAX_ENTRIES));
    }
    Ok(out)
}

#[derive(Deserialize)]
pub struct GlobArgs {
    pub pattern: String,
    pub path: Option<String>,
}

pub fn glob_files(a: GlobArgs) -> Result<String> {
    let root = resolve(a.path.as_deref().unwrap_or("."));
    let matcher = GlobBuilder::new(&a.pattern)
        .literal_separator(false)
        .build()
        .with_context(|| format!("invalid glob pattern: {}", a.pattern))?
        .compile_matcher();
    let mut results: Vec<String> = Vec::new();
    for entry in walk(&root) {
        let rel = rel_slash_path(entry.path(), &root);
        if matcher.is_match(&rel) {
            results.push(rel);
            if results.len() >= MAX_ENTRIES {
                break;
            }
        }
    }
    if results.is_empty() {
        return Ok(format!("No files matching '{}'", a.pattern));
    }
    results.sort();
    Ok(results.join("\n"))
}

/// Walk `root` yielding files only, skipping ignored directories.
pub fn walk(root: &Path) -> impl Iterator<Item = walkdir::DirEntry> {
    WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            !(e.file_type().is_dir()
                && e.depth() > 0
                && e.file_name().to_str().map(skip_dir).unwrap_or(false))
        })
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
}

pub fn rel_slash_path(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

const PREVIEW_LINES: usize = 30;
const MAX_DIFF_LINES: usize = 80;

/// Line-based unified diff with 2 context lines and line numbers.
/// Lines are prefixed '+' / '-' / ' ' so the UI can color them.
pub fn unified_diff(old: &str, new: &str) -> String {
    use similar::{ChangeTag, TextDiff};
    let diff = TextDiff::from_lines(old, new);
    let groups = diff.grouped_ops(2);
    if groups.is_empty() {
        return "(no changes)\n".into();
    }
    let mut s = String::new();
    let mut count = 0usize;
    for (gi, group) in groups.iter().enumerate() {
        if gi > 0 {
            s.push_str("  ...\n");
        }
        for op in group {
            for change in diff.iter_changes(op) {
                let (sign, num) = match change.tag() {
                    ChangeTag::Delete => ('-', change.old_index()),
                    ChangeTag::Insert => ('+', change.new_index()),
                    ChangeTag::Equal => (' ', change.old_index()),
                };
                let text = change.value();
                let text = text.strip_suffix('\n').unwrap_or(text);
                s.push_str(&format!(
                    "{sign}{:>5} {}\n",
                    num.map(|i| i + 1).unwrap_or(0),
                    text
                ));
                count += 1;
                if count >= MAX_DIFF_LINES {
                    s.push_str("  ... (diff truncated)\n");
                    return s;
                }
            }
        }
    }
    s
}

pub fn preview_write(args: &Value) -> String {
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("?");
    let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
    match std::fs::read_to_string(resolve(path)) {
        Ok(old) => format!("Overwrite {path}:\n{}", unified_diff(&old, content)),
        Err(_) => {
            let n = content.lines().count();
            let mut s = format!("Create {path} ({n} lines):\n");
            for (i, l) in content.lines().take(PREVIEW_LINES).enumerate() {
                s.push_str(&format!("+{:>5} {l}\n", i + 1));
            }
            if n > PREVIEW_LINES {
                s.push_str(&format!("  ... +{} more lines\n", n - PREVIEW_LINES));
            }
            s
        }
    }
}

pub fn preview_edit(args: &Value) -> String {
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("?");
    let old_s = args
        .get("old_string")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let new_s = args
        .get("new_string")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let replace_all = args
        .get("replace_all")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let header = format!("Edit {path}:\n");
    match std::fs::read_to_string(resolve(path)) {
        Ok(content) if !old_s.is_empty() && content.contains(old_s) => {
            let new_content = if replace_all {
                content.replace(old_s, new_s)
            } else {
                content.replacen(old_s, new_s, 1)
            };
            header + &unified_diff(&content, &new_content)
        }
        Ok(_) => {
            header
                + "(old_string not found in file, this edit will fail)\n"
                + &raw_edit_preview(old_s, new_s)
        }
        Err(_) => header + "(cannot read file)\n" + &raw_edit_preview(old_s, new_s),
    }
}

fn raw_edit_preview(old: &str, new: &str) -> String {
    let mut s = String::new();
    for l in old.lines().take(PREVIEW_LINES) {
        s.push_str(&format!("- {l}\n"));
    }
    if old.lines().count() > PREVIEW_LINES {
        s.push_str("- …\n");
    }
    for l in new.lines().take(PREVIEW_LINES) {
        s.push_str(&format!("+ {l}\n"));
    }
    if new.lines().count() > PREVIEW_LINES {
        s.push_str("+ …\n");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str, lines: usize) -> PathBuf {
        let dir = std::env::temp_dir().join("thoth-fs-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        let body: String = (1..=lines).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&p, body).unwrap();
        p
    }

    fn read(p: &Path, offset: usize, limit: usize) {
        read_file(ReadArgs {
            path: p.to_string_lossy().into_owned(),
            offset: Some(offset),
            limit: Some(limit),
        })
        .unwrap();
    }

    /// The hole found in review: peeking at the first and the last line used
    /// to count as having read the whole file.
    #[test]
    fn peeking_at_both_ends_does_not_unlock_overwrite() {
        let p = tmp("peek.txt", 500);
        read(&p, 1, 1);
        assert!(was_read(&p), "a peek still unlocks edit_file");
        read(&p, 500, 1);
        assert!(
            !was_fully_read(&p),
            "two 1-line peeks must not unlock write_file"
        );
        // reading the middle closes the gap
        read(&p, 2, 498);
        assert!(was_fully_read(&p), "contiguous coverage should unlock it");
    }

    #[test]
    fn reading_in_pages_unlocks_overwrite() {
        let p = tmp("pages.txt", 1000);
        read(&p, 1, 600);
        assert!(!was_fully_read(&p));
        read(&p, 601, 600);
        assert!(was_fully_read(&p), "sequential pages cover the file");
    }

    #[test]
    fn edits_to_the_file_reset_coverage() {
        let p = tmp("changed.txt", 10);
        read(&p, 1, 600);
        assert!(was_fully_read(&p));
        std::fs::write(&p, "a\n".repeat(40)).unwrap();
        read(&p, 1, 5);
        assert!(
            !was_fully_read(&p),
            "the file grew, old coverage must not carry over"
        );
    }

    #[test]
    fn refuses_to_slurp_a_huge_file() {
        let dir = std::env::temp_dir().join("thoth-fs-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("big.bin");
        std::fs::write(&p, vec![b'x'; (MAX_FILE_BYTES + 1) as usize]).unwrap();
        let err = read_file(ReadArgs {
            path: p.to_string_lossy().into_owned(),
            offset: None,
            limit: None,
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("too large"), "{err:#}");
    }
}
