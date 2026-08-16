use anyhow::{Context, Result, anyhow, bail};
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

/// Drops what was known about a file, so it has to be read again before it
/// can be changed. Used after an undo puts different content there.
pub fn forget_read(p: &Path) {
    read_registry().lock().unwrap().remove(&registry_key(p));
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
    let Ok(cwd) = cwd.canonicalize() else {
        return false;
    };
    if let Ok(t) = target.canonicalize() {
        return t.starts_with(cwd);
    }
    // The file does not exist yet, so judge the nearest parent that does.
    // Comparing the raw path against a canonical cwd never matches on
    // Windows, where canonicalize returns a \\?\ prefix.
    if path.contains("..") {
        return false;
    }
    let mut dir = target.parent();
    while let Some(d) = dir {
        if let Ok(real) = d.canonicalize() {
            return real.starts_with(&cwd);
        }
        dir = d.parent();
    }
    false
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

/// `cap` is the same budget the caller would truncate the result at. Doing
/// the cutting here instead means the reader still gets told which lines it
/// got and how to ask for the rest, which a blind truncation would take away
/// exactly when it matters most.
pub fn read_file(a: ReadArgs, cap: usize) -> Result<String> {
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

    // leave room for the footer that says where to carry on from
    let budget = cap.saturating_sub(80);
    let mut out = String::new();
    let mut shown = 0usize;
    let mut total = 0usize;
    let mut last_shown = 0usize;
    let mut full = false;
    for (i, line) in text.lines().enumerate() {
        let n = i + 1;
        total = n;
        if n < offset || shown >= limit || full {
            continue;
        }
        let line = if line.chars().count() > MAX_LINE_CHARS {
            let cut: String = line.chars().take(MAX_LINE_CHARS).collect();
            format!("{cut}…")
        } else {
            line.to_string()
        };
        if out.len() + line.len() + 8 > budget && shown > 0 {
            full = true;
            continue;
        }
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
            "(showing lines {}-{} of {}. read on with offset={})\n",
            offset.min(total),
            last_shown,
            total,
            last_shown + 1
        ));
    }
    Ok(out)
}

#[derive(Deserialize)]
pub struct WriteArgs {
    pub path: String,
    pub content: String,
    /// Add to the end of the file instead of replacing it.
    #[serde(default)]
    pub append: bool,
}

pub fn write_file(a: WriteArgs, cap: usize) -> Result<String> {
    let path = resolve(&a.path);
    let size = a.content.chars().count();
    if size > cap {
        bail!(
            "this call carries {size} characters, and one write_file may carry {cap} in this \
             context. A file this long does not survive being written in one go: the generation \
             is cut off before the call is finished and all of it is lost. Write it in parts. \
             Start with the first section, under {cap} characters, ending at a point the file is \
             still valid at, then send each next section to the same path with append true."
        );
    }
    // appending adds to a file without touching a line of what is already
    // there, so it needs no read: there is nothing to overwrite unseen
    if !a.append && path.exists() && !was_fully_read(&path) {
        bail!(
            "{} already exists and you have not read it completely. Read the whole file first, \
             and for modifying an existing file prefer edit_file over overwriting it. To add to \
             the end of it, call write_file again with append true",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create directory {}", parent.display()))?;
    }
    let before = crate::agent::undo::snapshot(&path);
    let old = match &before {
        crate::agent::undo::Before::Text(old) => Some(old.clone()),
        _ => None,
    };
    // an overwrite that flips a CRLF file to LF shows up as every line
    // changed, which is not what was asked for
    let added = match &old {
        Some(old) => align_newlines(old, &a.content),
        None => a.content.clone(),
    };
    let content = match (&old, a.append) {
        (Some(old), true) => {
            let mut s = old.clone();
            // a section that starts where the last one stopped mid-line is
            // a section pasted into the middle of a statement
            if !s.is_empty() && !s.ends_with('\n') {
                s.push_str(if s.contains("\r\n") { "\r\n" } else { "\n" });
            }
            s.push_str(&added);
            s
        }
        _ => added,
    };
    std::fs::write(&path, &content).with_context(|| format!("cannot write {}", path.display()))?;
    crate::agent::undo::record(&path, before);
    // the model authored the whole file, so it has seen all of it
    let lines = content.lines().count().max(1);
    mark_read(&path, 1, lines, lines);
    if a.append && old.is_some() {
        return Ok(format!(
            "Appended {} lines to {}, now {lines} lines ({} bytes)",
            a.content.lines().count(),
            path.display(),
            content.len()
        ));
    }
    Ok(format!(
        "Wrote {} lines ({} bytes) to {}",
        content.lines().count(),
        content.len(),
        path.display()
    ))
}

/// Text the model wrote, in the line endings the file already uses.
/// read_file hands out lines with the `\r` stripped, so a multi-line
/// old_string copied straight from it can never match a CRLF file byte for
/// byte. That is the tool's problem, not the model's, and finding out costs
/// a whole turn. The same alignment keeps a full overwrite from flipping
/// every line of a CRLF file to LF.
fn align_newlines(content: &str, s: &str) -> String {
    let flat = s.replace("\r\n", "\n");
    if content.contains("\r\n") {
        flat.replace('\n', "\r\n")
    } else {
        flat
    }
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
    empty_anchor(&a.old_string)?;
    let old = align_newlines(&content, &a.old_string);
    let new = align_newlines(&content, &a.new_string);
    let count = content.matches(&old).count();
    if count == 0 {
        bail!(
            "old_string not found in {}. read the file and copy the exact text",
            path.display()
        );
    }
    if !a.replace_all && count > 1 {
        return Err(ambiguous(
            &content,
            &old,
            count,
            &path.display().to_string(),
        ));
    }
    let new_content = if a.replace_all {
        content.replace(&old, &new)
    } else {
        content.replacen(&old, &new, 1)
    };
    std::fs::write(&path, new_content)
        .with_context(|| format!("cannot write {}", path.display()))?;
    crate::agent::undo::record(&path, crate::agent::undo::Before::Text(content));
    let n = if a.replace_all { count } else { 1 };
    Ok(format!("Edited {} ({n} replacement(s))", path.display()))
}

#[derive(Deserialize, Clone)]
pub struct EditStep {
    pub old_string: String,
    pub new_string: String,
    #[serde(default)]
    pub replace_all: bool,
}

#[derive(Deserialize)]
pub struct MultiEditArgs {
    pub path: String,
    pub edits: Vec<EditStep>,
}

/// Every edit to one file, or none of them. One call instead of one per
/// change saves a model round trip each time, which on a local model is the
/// difference between seconds and minutes, and it cannot leave the file half
/// edited the way a third call that fails would.
pub fn multi_edit(a: MultiEditArgs) -> Result<String> {
    let path = resolve(&a.path);
    if !was_read(&path) {
        bail!(
            "you have not read {} in this session. read it with read_file before editing",
            path.display()
        );
    }
    if a.edits.is_empty() {
        bail!("edits is empty: nothing to do");
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    let (new_content, n, skipped) = apply_edits(&content, &a.edits, &path.display().to_string())?;
    std::fs::write(&path, new_content)
        .with_context(|| format!("cannot write {}", path.display()))?;
    crate::agent::undo::record(&path, crate::agent::undo::Before::Text(content));
    let mut out = format!(
        "Edited {} ({} edits, {n} replacement(s))",
        path.display(),
        a.edits.len()
    );
    // never let the caller believe an edit landed when it asked for nothing
    if !skipped.is_empty() {
        let list: Vec<String> = skipped.iter().map(usize::to_string).collect();
        out.push_str(&format!(
            ". Edit {} asked for no change (old_string and new_string identical) and was \
             skipped; the rest were applied",
            list.join(", ")
        ));
    }
    Ok(out)
}

/// Shared by the tool and its preview, so what the user approves is exactly
/// what gets written. Fails on the first edit that does not apply, having
/// changed nothing on disk.
/// Where a snippet that appears more than once appears. "It is not unique,
/// add surrounding context" tells a model to guess; the line numbers tell it
/// which lines to look at, and it picks the right context in one turn
/// instead of two. Offsets come from `find` on this same string, so they are
/// on character boundaries by construction.
fn match_lines(content: &str, old: &str) -> Vec<usize> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    while let Some(i) = content[start..].find(old) {
        let at = start + i;
        lines.push(content[..at].matches('\n').count() + 1);
        start = at + old.len();
        if lines.len() >= 5 {
            break;
        }
    }
    lines
}

fn ambiguous(content: &str, old: &str, count: usize, name: &str) -> anyhow::Error {
    let lines: Vec<String> = match_lines(content, old)
        .iter()
        .map(usize::to_string)
        .collect();
    let more = if count > lines.len() { ", ..." } else { "" };
    anyhow!(
        "old_string appears {count} times in {name}, at line {}{more}. Take the one you mean and \
         add the lines above or below it until no other place matches, or set replace_all to \
         change every one",
        lines.join(", ")
    )
}

/// An empty old_string is a model reaching for an append with the wrong
/// tool. It matches between every pair of characters in the file, so what
/// it gets back otherwise is "old_string appears 61 times in Cargo.toml,
/// add surrounding context to make it unique", which is true and useless.
fn empty_anchor(old: &str) -> Result<()> {
    if old.is_empty() {
        bail!(
            "old_string is empty, so it matches everywhere. To add to the end of a file, call \
             write_file with append true. To change something in the middle, copy the exact \
             lines it should replace"
        );
    }
    Ok(())
}

/// Returns the new content, how many replacements were made, and which
/// edits were skipped because they asked for no change.
fn apply_edits(
    content: &str,
    edits: &[EditStep],
    name: &str,
) -> Result<(String, usize, Vec<usize>)> {
    let mut out = content.to_string();
    let mut total = 0usize;
    let mut skipped: Vec<usize> = Vec::new();
    if edits.iter().all(|e| e.old_string == e.new_string) {
        bail!("every edit has old_string equal to new_string: nothing to do");
    }
    for (i, e) in edits.iter().enumerate() {
        let step = i + 1;
        // one edit that changes nothing is not a reason to throw away the
        // ones that do: applying none of them is for an edit that fails,
        // and this one cannot fail, it just has no work in it
        if e.old_string == e.new_string {
            skipped.push(step);
            continue;
        }
        empty_anchor(&e.old_string).with_context(|| format!("edit {step}"))?;
        let old = align_newlines(&out, &e.old_string);
        let new = align_newlines(&out, &e.new_string);
        let count = out.matches(&old).count();
        if count == 0 {
            bail!(
                "edit {step}: old_string not found in {name}. copy the exact text, and remember \
                 that an earlier edit in this same call may have changed it"
            );
        }
        if !e.replace_all && count > 1 {
            return Err(ambiguous(&out, &old, count, name).context(format!("edit {step}")));
        }
        out = if e.replace_all {
            total += count;
            out.replace(&old, &new)
        } else {
            total += 1;
            out.replacen(&old, &new, 1)
        };
    }
    Ok((out, total, skipped))
}

/// The whole set of edits as one diff: what the user approves is the file
/// they will end up with, not a list of snippets to apply in their head.
pub fn preview_multi_edit(args: &Value) -> String {
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("?");
    let edits: Vec<EditStep> = args
        .get("edits")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    let header = format!("Edit {path} ({} edits):\n", edits.len());
    let Ok(content) = std::fs::read_to_string(resolve(path)) else {
        return header + "(cannot read file)\n";
    };
    match apply_edits(&content, &edits, path) {
        Ok((new_content, _, skipped)) => {
            let mut s = header;
            if !skipped.is_empty() {
                let list: Vec<String> = skipped.iter().map(usize::to_string).collect();
                s.push_str(&format!(
                    "(edit {} changes nothing and is skipped)\n",
                    list.join(", ")
                ));
            }
            s + &unified_diff(&content, &new_content)
        }
        Err(e) => format!("{header}({e:#}, this call will fail)\n"),
    }
}

#[derive(Deserialize)]
pub struct MoveArgs {
    pub from: String,
    pub to: String,
}

/// Renaming keeps the content, so the read record moves with the file: a
/// file you had read stays editable under its new name.
pub fn move_file(a: MoveArgs) -> Result<String> {
    let from = resolve(&a.from);
    let to = resolve(&a.to);
    if !from.is_file() {
        bail!("{} is not a file", from.display());
    }
    if !inside_project(&a.from) || !inside_project(&a.to) {
        bail!(
            "move_file only works inside the working directory. moving a file out of it, or \
             something from elsewhere into it, is not thoth's to do"
        );
    }
    // a moved-away file leaves its old path free, and write_file lets anyone
    // create a file that does not exist. without this, renaming would be the
    // way around having to read a file before overwriting it
    if !was_fully_read(&from) {
        bail!(
            "you have not read all of {} in this session. read it first: after a move its old \
             path is free for write_file, so this is the same as overwriting it",
            from.display()
        );
    }
    if to.exists() {
        bail!(
            "{} already exists. delete it first if that is really what you want",
            to.display()
        );
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create directory {}", parent.display()))?;
    }
    // the key has to be taken while the file is still there: canonicalize
    // fails on a path that no longer exists, and the fallback would not
    // match what was stored
    let old_key = registry_key(&from);
    let before = crate::agent::undo::snapshot(&from);
    std::fs::rename(&from, &to)
        .with_context(|| format!("cannot move {} to {}", from.display(), to.display()))?;
    // undo puts the content back where it was and takes away the new copy
    crate::agent::undo::record(&to, crate::agent::undo::Before::Missing);
    crate::agent::undo::record(&from, before);
    let mut r = read_registry().lock().unwrap();
    if let Some(state) = r.remove(&old_key) {
        r.insert(registry_key(&to), state);
    }
    Ok(format!("Moved {} to {}", from.display(), to.display()))
}

#[derive(Deserialize)]
pub struct DeleteArgs {
    pub path: String,
}

/// Deleting a file you have not read is exactly as destructive as
/// overwriting one, so it carries the same condition: the whole file must
/// have been read this session. That also closes the back door of deleting a
/// file and writing a fresh one over it to dodge the read rule.
pub fn delete_file(a: DeleteArgs) -> Result<String> {
    let path = resolve(&a.path);
    if path.is_dir() {
        bail!(
            "{} is a directory. thoth only deletes single files; ask the user to remove a \
             directory themselves",
            path.display()
        );
    }
    if !path.is_file() {
        bail!("{} does not exist", path.display());
    }
    if !inside_project(&a.path) {
        bail!(
            "{} is outside the working directory. thoth does not delete files there",
            path.display()
        );
    }
    if !was_fully_read(&path) {
        bail!(
            "you have not read all of {} in this session. read it first: deleting a file you \
             have not seen is not something to do on a guess",
            path.display()
        );
    }
    let before = crate::agent::undo::snapshot(&path);
    std::fs::remove_file(&path).with_context(|| format!("cannot delete {}", path.display()))?;
    crate::agent::undo::record(&path, before);
    read_registry().lock().unwrap().remove(&registry_key(&path));
    Ok(format!("Deleted {}", path.display()))
}

/// What is about to be lost, so the answer to the prompt is an informed one.
/// This runs before the user has approved anything, so it reads only what
/// the call itself would be allowed to touch, and only a little of it.
pub fn preview_delete(args: &Value) -> String {
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("?");
    let head = if !inside_project(path) {
        "(outside the working directory, this call will fail)\n".to_string()
    } else {
        match head_of(&resolve(path), 5) {
            Some(text) => text,
            None => "(cannot read it)\n".to_string(),
        }
    };
    format!("Delete {path} {head}")
}

/// First `n` lines of a file, without pulling the whole thing into memory:
/// a preview must not be a way to read a gigabyte.
fn head_of(path: &Path, n: usize) -> Option<String> {
    use std::io::{BufRead, BufReader};
    let file = std::fs::File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    let mut out = String::new();
    for line in BufReader::new(file).lines().take(n) {
        let line = line.ok()?;
        let line: String = line.chars().take(MAX_LINE_CHARS).collect();
        out.push_str(&format!("  {line}\n"));
    }
    Some(format!("({size} bytes)\n{out}"))
}

#[derive(Deserialize)]
pub struct ListArgs {
    pub path: Option<String>,
}

pub fn list_dir(a: ListArgs, cap: usize) -> Result<String> {
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
    Ok(fit_entries(all, cap))
}

/// As many entries as fit the budget, with a line saying what was left out.
/// Counting entries alone is not enough: 500 long paths overrun a small
/// window, and the note explaining that would be the first thing cut.
fn fit_entries(all: Vec<String>, cap: usize) -> String {
    let room = cap.saturating_sub(40);
    let total = all.len();
    let mut out = String::new();
    let mut shown = 0usize;
    for e in all.into_iter().take(MAX_ENTRIES) {
        if out.len() + e.len() + 1 > room && shown > 0 {
            break;
        }
        if shown > 0 {
            out.push('\n');
        }
        out.push_str(&e);
        shown += 1;
    }
    if total > shown {
        out.push_str(&format!("\n… ({} more entries)", total - shown));
    }
    out
}

#[derive(Deserialize)]
pub struct GlobArgs {
    pub pattern: String,
    pub path: Option<String>,
}

pub fn glob_files(a: GlobArgs, cap: usize) -> Result<String> {
    let root = resolve(a.path.as_deref().unwrap_or("."));
    let matcher = GlobBuilder::new(&a.pattern)
        .literal_separator(false)
        .build()
        .with_context(|| format!("invalid glob pattern: {}", a.pattern))?
        .compile_matcher();
    let mut results: Vec<String> = Vec::new();
    let mut more = false;
    for entry in walk(&root) {
        let rel = rel_slash_path(entry.path(), &root);
        if matcher.is_match(&rel) {
            if results.len() >= MAX_ENTRIES {
                // stopping is fine, stopping quietly is not: the caller would
                // read a full-looking list and believe that is all there is
                more = true;
                break;
            }
            results.push(rel);
        }
    }
    if results.is_empty() {
        return Ok(format!("No files matching '{}'", a.pattern));
    }
    results.sort();
    let out = fit_entries(results, cap);
    Ok(match more {
        true => format!("{out}\n… (stopped at {MAX_ENTRIES} matches; narrow the pattern)"),
        false => out,
    })
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
    let append = args
        .get("append")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if append && std::fs::metadata(resolve(path)).is_ok() {
        let n = content.lines().count();
        let mut s = format!("Append to {path} ({n} lines):\n");
        for l in content.lines().take(PREVIEW_LINES) {
            s.push_str(&format!("+{l}\n"));
        }
        if n > PREVIEW_LINES {
            s.push_str(&format!("  ... +{} more lines\n", n - PREVIEW_LINES));
        }
        return s;
    }
    match std::fs::read_to_string(resolve(path)) {
        // through the same alignment write_file uses, or a CRLF file would
        // preview as every single line changed
        Ok(old) => format!(
            "Overwrite {path}:\n{}",
            unified_diff(&old, &align_newlines(&old, content))
        ),
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
        Ok(content) if !old_s.is_empty() && content.contains(&align_newlines(&content, old_s)) => {
            let (old_s, new_s) = (
                align_newlines(&content, old_s),
                align_newlines(&content, new_s),
            );
            let new_content = if replace_all {
                content.replace(&old_s, &new_s)
            } else {
                content.replacen(&old_s, &new_s, 1)
            };
            header + &unified_diff(&content, &new_content)
        }
        // the preview has to give the same reason the tool will: "not
        // found" for an empty anchor sends the user looking for a typo in
        // a string that is not there at all
        Ok(_) if old_s.is_empty() => {
            header
                + "(old_string is empty, so this edit will fail. Adding to the end of a file is \
                   write_file with append)\n"
                + &raw_edit_preview(old_s, new_s)
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
        read_file(
            ReadArgs {
                path: p.to_string_lossy().into_owned(),
                offset: Some(offset),
                limit: Some(limit),
            },
            12_000,
        )
        .unwrap();
    }

    /// delete_file only works inside the working directory, so this one
    /// cannot use the shared temp dir. `target/` is ignored by git.
    fn tmp_in_project(name: &str, lines: usize) -> PathBuf {
        let dir = std::env::current_dir().unwrap().join("target/fs-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        let body: String = (1..=lines).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&p, body).unwrap();
        p
    }

    /// Deleting a file is as destructive as overwriting it, and it used to be
    /// the way around the read rule: remove the file, then write a fresh one
    /// where it stood. Both need the file to have been read.
    #[test]
    fn deleting_needs_the_file_to_have_been_read() {
        let p = tmp_in_project("del.txt", 5);
        let args = || DeleteArgs {
            path: p.to_string_lossy().into_owned(),
        };
        let err = delete_file(args()).unwrap_err();
        assert!(format!("{err:#}").contains("read"), "{err:#}");
        assert!(p.exists(), "nothing may be deleted on a guess");

        read(&p, 1, 600);
        delete_file(args()).unwrap();
        assert!(!p.exists());
    }

    /// Found in review: a rename leaves the old path free, and write_file is
    /// allowed to create a file that does not exist. Without this, moving a
    /// file away was the way around having to read it before overwriting it.
    #[test]
    fn moving_needs_the_file_to_have_been_read() {
        let from = tmp_in_project("unread-move.txt", 3);
        let to = from.with_file_name("moved.txt");
        let _ = std::fs::remove_file(&to);
        let err = move_file(MoveArgs {
            from: from.to_string_lossy().into_owned(),
            to: to.to_string_lossy().into_owned(),
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("read"), "{err:#}");
        assert!(from.exists() && !to.exists());
    }

    /// Also found in review: without this, `move_file` could carry a file in
    /// from anywhere, and reads inside the project are automatic.
    #[test]
    fn moving_stays_inside_the_project() {
        let outside = tmp("outside.txt", 2);
        let inside = tmp_in_project("landed.txt", 1);
        let _ = std::fs::remove_file(&inside);
        read(&outside, 1, 600);
        let err = move_file(MoveArgs {
            from: outside.to_string_lossy().into_owned(),
            to: inside.to_string_lossy().into_owned(),
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("working directory"), "{err:#}");
        assert!(outside.exists() && !inside.exists());

        // and the other way: nothing is carried out of the project either
        let from = tmp_in_project("leaving.txt", 1);
        read(&from, 1, 600);
        let err = move_file(MoveArgs {
            from: from.to_string_lossy().into_owned(),
            to: tmp("escaped.txt", 0).to_string_lossy().into_owned(),
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("working directory"), "{err:#}");
        assert!(from.exists());
    }

    /// The preview runs before the user has approved anything, so it must not
    /// read what the call itself would refuse to touch.
    #[test]
    fn the_delete_preview_does_not_read_outside_the_project() {
        let outside = tmp("secret.txt", 3);
        std::fs::write(&outside, "line 1\nsensitive\n").unwrap();
        let out = preview_delete(&serde_json::json!({
            "path": outside.to_string_lossy()
        }));
        assert!(out.contains("outside the working directory"), "{out}");
        assert!(!out.contains("sensitive"), "it read the file anyway: {out}");
    }

    #[test]
    fn a_directory_is_never_deleted() {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target/fs-tests/a-dir");
        std::fs::create_dir_all(&dir).unwrap();
        let err = delete_file(DeleteArgs {
            path: dir.to_string_lossy().into_owned(),
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("directory"), "{err:#}");
        assert!(dir.exists());
    }

    #[test]
    fn moving_takes_the_read_record_with_it() {
        let from = tmp_in_project("before.txt", 4);
        let to = from.with_file_name("after.txt");
        let _ = std::fs::remove_file(&to);
        read(&from, 1, 600);
        move_file(MoveArgs {
            from: from.to_string_lossy().into_owned(),
            to: to.to_string_lossy().into_owned(),
        })
        .unwrap();
        assert!(!from.exists() && to.exists());
        // the content is the same content, so it stays editable
        assert!(was_fully_read(&to), "the read record did not follow");
        assert!(!was_read(&from));
    }

    #[test]
    fn moving_never_overwrites() {
        let from = tmp_in_project("src.txt", 2);
        let to = tmp_in_project("dst.txt", 2);
        read(&from, 1, 600);
        let err = move_file(MoveArgs {
            from: from.to_string_lossy().into_owned(),
            to: to.to_string_lossy().into_owned(),
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("already exists"), "{err:#}");
        assert!(from.exists() && to.exists());
    }

    fn steps(v: &[(&str, &str)]) -> Vec<EditStep> {
        v.iter()
            .map(|(o, n)| EditStep {
                old_string: (*o).to_string(),
                new_string: (*n).to_string(),
                replace_all: false,
            })
            .collect()
    }

    #[test]
    fn several_edits_land_in_one_pass() {
        let p = tmp("multi.txt", 0);
        std::fs::write(&p, "one\ntwo\nthree\n").unwrap();
        read(&p, 1, 600);
        let out = multi_edit(MultiEditArgs {
            path: p.to_string_lossy().into_owned(),
            edits: steps(&[("one", "1"), ("three", "3")]),
        })
        .unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "1\ntwo\n3\n");
        assert!(out.contains("2 edits"), "{out}");
    }

    /// The point of doing them together: a set that cannot be applied leaves
    /// the file exactly as it was, instead of half changed the way three
    /// separate calls would when the third one fails.
    #[test]
    fn one_bad_edit_writes_nothing() {
        let p = tmp("atomic.txt", 0);
        std::fs::write(&p, "alpha\nbeta\n").unwrap();
        read(&p, 1, 600);
        let err = multi_edit(MultiEditArgs {
            path: p.to_string_lossy().into_owned(),
            edits: steps(&[("alpha", "A"), ("nowhere", "X")]),
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("edit 2"), "{err:#}");
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "alpha\nbeta\n",
            "the file must be untouched"
        );
    }

    /// "Not unique, add surrounding context" leaves the model guessing which
    /// of the matches it should have been looking at.
    #[test]
    fn an_ambiguous_anchor_says_which_lines_match() {
        let p = tmp("ambiguous.rs", 0);
        std::fs::write(
            &p,
            "fn first() -> i32 {\n    let x = 1;\n    x\n}\n\nfn second() -> i32 {\n    let x = 1;\n    x\n}\n",
        )
        .unwrap();
        read(&p, 1, 600);
        let err = edit_file(EditArgs {
            path: p.to_string_lossy().into_owned(),
            old_string: "    let x = 1;".into(),
            new_string: "    let x = 2;".into(),
            replace_all: false,
        })
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("appears 2 times"), "{msg}");
        assert!(
            msg.contains("at line 2, 7"),
            "the lines are the help: {msg}"
        );
        assert!(msg.contains("replace_all"), "{msg}");
    }

    /// An empty anchor is an append written with the wrong tool, and the
    /// count of how many times nothing appears in the file says nothing
    /// about that.
    #[test]
    fn an_empty_old_string_points_at_append() {
        let p = tmp("empty_anchor.toml", 0);
        std::fs::write(&p, "[package]\nname = \"probe\"\n").unwrap();
        read(&p, 1, 600);
        let err = edit_file(EditArgs {
            path: p.to_string_lossy().into_owned(),
            old_string: String::new(),
            new_string: "[[bin]]\n".into(),
            replace_all: false,
        })
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("append true"), "{msg}");
        assert!(!msg.contains("appears"), "not a uniqueness problem: {msg}");

        let err = multi_edit(MultiEditArgs {
            path: p.to_string_lossy().into_owned(),
            edits: steps(&[("", "[[bin]]\n")]),
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("edit 1"), "{err:#}");
    }

    /// One edit that asks for no change is not a failed edit, and throwing
    /// away the three real ones next to it costs a whole turn on a model
    /// that only got one of four wrong.
    #[test]
    fn an_edit_that_changes_nothing_is_skipped_not_fatal() {
        let p = tmp("noop.txt", 0);
        std::fs::write(&p, "alpha\nbeta\ngamma\n").unwrap();
        read(&p, 1, 600);
        let out = multi_edit(MultiEditArgs {
            path: p.to_string_lossy().into_owned(),
            edits: steps(&[("alpha", "A"), ("beta", "beta"), ("gamma", "G")]),
        })
        .unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "A\nbeta\nG\n");
        assert!(
            out.contains("Edit 2"),
            "it has to say what it skipped: {out}"
        );

        // but a call with nothing in it at all is still an error
        let err = multi_edit(MultiEditArgs {
            path: p.to_string_lossy().into_owned(),
            edits: steps(&[("A", "A")]),
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("nothing to do"), "{err:#}");
    }

    #[test]
    fn a_later_edit_sees_what_an_earlier_one_did() {
        let p = tmp("chain.txt", 0);
        std::fs::write(&p, "value = 1\n").unwrap();
        read(&p, 1, 600);
        multi_edit(MultiEditArgs {
            path: p.to_string_lossy().into_owned(),
            edits: steps(&[("value = 1", "value = 2"), ("value = 2", "value = 3")]),
        })
        .unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "value = 3\n");
    }

    #[test]
    fn it_obeys_read_before_write_like_every_other_edit() {
        let p = tmp("unread.txt", 0);
        std::fs::write(&p, "hello\n").unwrap();
        let err = multi_edit(MultiEditArgs {
            path: p.to_string_lossy().into_owned(),
            edits: steps(&[("hello", "bye")]),
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("read"), "{err:#}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "hello\n");
    }

    #[test]
    fn the_preview_is_the_whole_change_as_one_diff() {
        let p = tmp("preview.txt", 0);
        std::fs::write(&p, "a\nb\n").unwrap();
        let out = preview_multi_edit(&serde_json::json!({
            "path": p.to_string_lossy(),
            "edits": [
                {"old_string": "a", "new_string": "A"},
                {"old_string": "b", "new_string": "B"}
            ]
        }));
        assert!(out.contains("2 edits"), "{out}");
        assert!(out.contains('A') && out.contains('B'), "{out}");
        // and a set that cannot apply says so instead of showing a lie
        let bad = preview_multi_edit(&serde_json::json!({
            "path": p.to_string_lossy(),
            "edits": [{"old_string": "zzz", "new_string": "!"}]
        }));
        assert!(bad.contains("will fail"), "{bad}");
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

    /// The budget used to be applied twice: read_file cut at 600 lines and
    /// the caller cut the text at a fixed 12k characters, which threw away
    /// read_file's own footer. Losing that footer is what left the model not
    /// knowing there was more of the file, or how to ask for it.
    /// A cut-off list that says nothing reads exactly like a complete one.
    #[test]
    fn glob_says_when_it_stopped_counting() {
        let dir = std::env::temp_dir().join("thoth-fs-tests/many");
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..MAX_ENTRIES + 20 {
            std::fs::write(dir.join(format!("f{i}.txt")), "x").unwrap();
        }
        let out = glob_files(
            GlobArgs {
                pattern: "*.txt".into(),
                path: Some(dir.to_string_lossy().into_owned()),
            },
            100_000,
        )
        .unwrap();
        assert!(out.contains("stopped at"), "{}", &out[out.len() - 200..]);
    }

    /// The preview is what the user approves, so it has to be the change
    /// that will actually be made, not one line-ending style against another.
    #[test]
    fn overwriting_a_crlf_file_previews_only_the_real_change() {
        let p = tmp("preview-crlf.txt", 3);
        std::fs::write(&p, "one\r\ntwo\r\nthree\r\n").unwrap();
        let preview = preview_write(&serde_json::json!({
            "path": p.to_string_lossy(),
            "content": "one\ntwo and a half\nthree\n",
        }));
        assert!(preview.contains("+    2 two and a half"), "{preview}");
        assert!(
            !preview.contains("-    1 one"),
            "unchanged lines must not show as changed:\n{preview}"
        );
    }

    /// read_file strips the `\r`, so a multi-line old_string copied from it
    /// is LF while the file is CRLF, and every multi-line edit on a Windows
    /// checkout fails to match. The tool has to meet the file where it is.
    #[test]
    fn a_crlf_file_can_be_edited_and_stays_crlf() {
        let p = tmp("crlf.txt", 3);
        std::fs::write(&p, "one\r\ntwo\r\nthree\r\n").unwrap();
        read_file(
            ReadArgs {
                path: p.to_string_lossy().into_owned(),
                offset: None,
                limit: None,
            },
            12_000,
        )
        .unwrap();
        // the model copies what it was shown: no carriage returns in it
        edit_file(EditArgs {
            path: p.to_string_lossy().into_owned(),
            old_string: "one\ntwo".into(),
            new_string: "one\ntwo and a half".into(),
            replace_all: false,
        })
        .expect("a CRLF file must be editable");
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "one\r\ntwo and a half\r\nthree\r\n",
            "the file's own line endings must survive the edit"
        );

        // and a full overwrite keeps them too, instead of turning every
        // line of the file into a change
        write_file(
            WriteArgs {
                path: p.to_string_lossy().into_owned(),
                content: "alpha\nbeta\n".into(),
                append: false,
            },
            10_000,
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "alpha\r\nbeta\r\n");
    }

    /// Sections of a long file go on the end without a read and without an
    /// old_string to make unique, which is the whole point: the last lines
    /// of a Rust file are `}` and `}`, and no anchor built out of them is
    /// ever unique.
    #[test]
    fn a_section_is_appended_to_what_is_already_there() {
        let dir = std::env::temp_dir().join("thoth_append_test");
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("json.rs");
        let _ = std::fs::remove_file(&p);
        let args = |content: &str, append: bool| WriteArgs {
            path: p.to_string_lossy().into_owned(),
            content: content.into(),
            append,
        };

        write_file(args("fn a() {}\n", false), 10_000).unwrap();
        forget_read(&p);

        // no read in between, the way a second section arrives
        let out = write_file(args("fn b() {}\n", true), 10_000).unwrap();
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "fn a() {}\nfn b() {}\n"
        );
        assert!(out.contains("Appended 1 lines"), "{out}");
        assert!(out.contains("now 2 lines"), "{out}");

        // an overwrite in the same spot is still refused
        forget_read(&p);
        let err = write_file(args("fn c() {}\n", false), 10_000).unwrap_err();
        assert!(format!("{err:#}").contains("append true"), "{err:#}");

        // a section that lands on a file with no trailing newline does not
        // get glued onto the last line of it
        std::fs::write(&p, "fn a() {}").unwrap();
        write_file(args("fn b() {}\n", true), 10_000).unwrap();
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "fn a() {}\nfn b() {}\n"
        );

        // and the file's own line endings survive
        std::fs::write(&p, "fn a() {}\r\n").unwrap();
        write_file(args("fn b() {}\n", true), 10_000).unwrap();
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "fn a() {}\r\nfn b() {}\r\n"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// A file too long to generate in one turn is refused before it can
    /// take the turn down with it, and the refusal says how to write it.
    #[test]
    fn a_write_too_big_for_the_window_is_refused() {
        let dir = std::env::temp_dir().join("thoth_write_cap_test");
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("big.rs");
        let args = |content: String| WriteArgs {
            path: p.to_string_lossy().into_owned(),
            content,
            append: false,
        };

        let err = write_file(args("x".repeat(3_000)), 2_000).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("3000 characters"), "{msg}");
        assert!(
            msg.contains("append true"),
            "it has to say what to do: {msg}"
        );
        assert!(
            !p.exists(),
            "nothing may be written when the call is refused"
        );

        // and the same content is fine where there is room for it
        write_file(args("x".repeat(3_000)), 4_000).unwrap();
        assert!(p.exists());
        let _ = std::fs::remove_file(&p);
    }

    /// The same double-cap trap read_file had: a listing that overran the
    /// caller's budget got cut blind, taking the "there is more" note with
    /// it. It has to fit and still say what was left out.
    #[test]
    fn a_listing_fits_the_budget_and_says_what_it_left_out() {
        let many: Vec<String> = (0..300)
            .map(|i| format!("some/long/path/file-{i}.rs"))
            .collect();
        let out = fit_entries(many.clone(), 400);
        assert!(out.len() <= 400, "over budget: {}", out.len());
        assert!(out.contains("more entries"), "{out}");
        // room for everything: no note, nothing dropped
        let out = fit_entries(many, 100_000);
        assert!(!out.contains("more entries"), "{out}");
        assert_eq!(out.lines().count(), 300);
    }

    #[test]
    fn a_small_budget_still_says_where_to_carry_on() {
        let p = tmp("budget.txt", 1000);
        let out = read_file(
            ReadArgs {
                path: p.to_string_lossy().into_owned(),
                offset: None,
                limit: None,
            },
            600,
        )
        .unwrap();
        assert!(out.len() <= 600, "the cap was not respected: {}", out.len());
        assert!(out.contains("of 1000"), "no footer: {out}");
        assert!(out.contains("read on with offset="), "{out}");
        // and what it says is true: reading on from there works
        let next: usize = out
            .rsplit("read on with offset=")
            .next()
            .unwrap()
            .trim_end_matches([')', '\n'])
            .parse()
            .expect("the offset it prints has to be a number");
        assert!(next > 1 && next <= 1000, "{next}");
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
        let err = read_file(
            ReadArgs {
                path: p.to_string_lossy().into_owned(),
                offset: None,
                limit: None,
            },
            12_000,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("too large"), "{err:#}");
    }
}
