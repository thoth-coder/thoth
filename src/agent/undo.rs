//! What a file looked like before thoth changed it, so the user has a way
//! back. An agent that edits, moves and deletes real files needs an undo
//! more than it needs another rule telling it to be careful.
//!
//! One checkpoint per request, holding every file that request touched.
//! `/undo` restores the newest one, then the one before it. Checkpoints live
//! under `~/.thoth/projects/<key>/undo/`, so they survive a crash, which is
//! exactly when they are wanted.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Checkpoints kept before the oldest is dropped.
const KEEP: usize = 20;
/// A single request's snapshot is not allowed to grow past this; a request
/// that rewrites half a repository is not what undo is for.
const MAX_SNAPSHOT_BYTES: usize = 4_000_000;

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub enum Before {
    /// The file existed and this was in it.
    Text(String),
    /// The file was not there: undoing means deleting it again.
    Missing,
    /// Something was there that could not be read back as text. Undo leaves
    /// it alone rather than guessing.
    Unreadable,
    /// The request had already spent its snapshot budget when this file was
    /// changed. Recorded anyway, so `/undo` says the file is still changed
    /// instead of quietly leaving it out of the report.
    TooLarge,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Change {
    pub path: PathBuf,
    pub before: Before,
    /// Hash of what thoth left behind, so a file the user has edited since
    /// is never silently overwritten.
    pub after: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
pub struct Checkpoint {
    /// The request that caused it, shortened, so the user knows what they
    /// are undoing.
    pub label: String,
    pub changes: Vec<Change>,
    /// Already undone. A spent checkpoint stays on disk instead of being
    /// deleted: for a file that had changed since and was left alone, this
    /// is the only copy of what was in it before.
    #[serde(default)]
    pub spent: bool,
}

fn pending() -> &'static Mutex<Vec<Change>> {
    static P: OnceLock<Mutex<Vec<Change>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(Vec::new()))
}

/// Recording is off until an agent turns it on. The file tools are a library
/// anyone can call; only a running session has requests to group changes
/// into, or a `/undo` to spend them on.
static ARMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn arm() {
    ARMED.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn armed() -> bool {
    ARMED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Where checkpoints live. The tests point this somewhere temporary: a test
/// run must not write into the state of whatever project it runs in.
fn dir_override() -> &'static OnceLock<PathBuf> {
    static D: OnceLock<PathBuf> = OnceLock::new();
    &D
}

fn undo_dir() -> PathBuf {
    match dir_override().get() {
        Some(d) => d.clone(),
        None => crate::tools::memory::project_dir().join("undo"),
    }
}

fn hash(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn hash_of_file(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|c| hash(&c))
}

/// Called by a tool just after it changed a file. `before` is what was there
/// first; the state it leaves behind is read from disk.
pub fn record(path: &Path, before: Before) {
    if !armed() {
        return;
    }
    let mut p = pending().lock().unwrap();
    // over budget: keep the entry, drop the content. An undo that silently
    // leaves one file changed is worse than one that says it did
    let before = if fits(&p, &before) {
        before
    } else {
        Before::TooLarge
    };
    p.push(Change {
        path: path.to_path_buf(),
        before,
        after: hash_of_file(path),
    });
}

/// Whether one more snapshot still fits in a request's budget. A request
/// that rewrites half a repository stops being snapshotted rather than
/// filling the disk with copies of it.
fn fits(taken: &[Change], next: &Before) -> bool {
    let size = |b: &Before| match b {
        Before::Text(t) => t.len(),
        _ => 0,
    };
    taken.iter().map(|c| size(&c.before)).sum::<usize>() + size(next) <= MAX_SNAPSHOT_BYTES
}

/// Reads a file the way `record` wants it, before the change happens.
pub fn snapshot(path: &Path) -> Before {
    match std::fs::read_to_string(path) {
        Ok(text) => Before::Text(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Before::Missing,
        Err(_) => Before::Unreadable,
    }
}

/// Closes off the request: everything recorded since the last commit becomes
/// one checkpoint. Returns how many files it covers.
pub fn commit(label: &str) -> usize {
    let changes: Vec<Change> = std::mem::take(&mut *pending().lock().unwrap());
    write_checkpoint(label, changes)
}

/// Split out so the tests can hand it exactly the changes they mean, rather
/// than whatever else happens to be pending in the same process.
fn write_checkpoint(label: &str, changes: Vec<Change>) -> usize {
    if changes.is_empty() {
        return 0;
    }
    let n = changes.len();
    let cp = Checkpoint {
        label: label.chars().take(60).collect(),
        changes,
        spent: false,
    };
    let dir = undo_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return 0;
    }
    // the name is the ordering: seconds since the epoch, plus a counter for
    // two requests that land in the same second
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // every name has the same shape, so sorting them by text is sorting them
    // by time. A bare "…000.json" next to "…000-1.json" would not be: '-'
    // sorts before '.', and /undo would reach for the older one
    let mut extra = 0;
    let mut path = dir.join(format!("{stamp:012}-{extra:04}.json"));
    while path.exists() {
        extra += 1;
        path = dir.join(format!("{stamp:012}-{extra:04}.json"));
    }
    if let Ok(json) = serde_json::to_string(&cp) {
        let _ = std::fs::write(&path, json);
    }
    prune();
    n
}

/// Only the tests need this: a real request always commits what it changed,
/// including one that was interrupted half way.
#[cfg(test)]
pub fn forget_pending() {
    pending().lock().unwrap().clear();
}

fn checkpoint_files() -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(undo_dir()) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    files
}

fn prune() {
    let files = checkpoint_files();
    for old in files.iter().take(files.len().saturating_sub(KEEP)) {
        let _ = std::fs::remove_file(old);
    }
}

#[derive(Debug)]
pub struct Restored {
    pub label: String,
    pub restored: Vec<String>,
    /// Files left alone, with the reason.
    pub skipped: Vec<String>,
}

/// Puts the newest checkpoint back. A file that changed after thoth wrote it
/// is left alone and reported: someone else's edit is not ours to discard.
pub fn undo_last() -> Result<Restored> {
    let Some((newest, mut cp)) = newest_unspent() else {
        bail!("nothing to undo in this project");
    };
    let mut out = Restored {
        label: cp.label.clone(),
        restored: Vec::new(),
        skipped: Vec::new(),
    };
    for c in cp.changes.iter().rev() {
        let shown = c.path.display().to_string();
        let now = hash_of_file(&c.path);
        if now != c.after {
            out.skipped
                .push(format!("{shown} (changed since, left alone)"));
            continue;
        }
        match &c.before {
            Before::Unreadable => out
                .skipped
                .push(format!("{shown} (was not text, left alone)")),
            Before::TooLarge => out
                .skipped
                .push(format!("{shown} (too large to snapshot, still changed)")),
            Before::Missing => match std::fs::remove_file(&c.path) {
                Ok(()) => out.restored.push(format!("{shown} (removed again)")),
                Err(e) => out.skipped.push(format!("{shown} ({e})")),
            },
            Before::Text(t) => match std::fs::write(&c.path, t) {
                Ok(()) => {
                    crate::tools::fs::forget_read(&c.path);
                    out.restored.push(shown);
                }
                Err(e) => out.skipped.push(format!("{shown} ({e})")),
            },
        }
    }
    // spent, not deleted: what a skipped file held before is only in here
    cp.spent = true;
    match serde_json::to_string(&cp) {
        Ok(json) => {
            let _ = std::fs::write(&newest, json);
        }
        Err(_) => {
            let _ = std::fs::remove_file(&newest);
        }
    }
    Ok(out)
}

/// The newest checkpoint that has not been undone yet, with its file. A
/// checkpoint that will not parse is stepped over rather than blocking undo.
fn newest_unspent() -> Option<(PathBuf, Checkpoint)> {
    checkpoint_files().iter().rev().find_map(|p| {
        let text = std::fs::read_to_string(p).ok()?;
        let cp: Checkpoint = serde_json::from_str(&text).ok()?;
        (!cp.spent).then(|| (p.clone(), cp))
    })
}

/// Newest first, for `/undo list`.
pub fn list() -> Vec<String> {
    let mut out: Vec<String> = checkpoint_files()
        .iter()
        .rev()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .filter_map(|t| serde_json::from_str::<Checkpoint>(&t).ok())
        .map(|c| {
            format!(
                "{} file(s)  {}{}",
                c.changes.len(),
                c.label,
                if c.spent { "  (undone)" } else { "" }
            )
        })
        .collect();
    out.truncate(KEEP);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These tests share the pending list and the checkpoint directory, so
    /// they take turns, and they keep their checkpoints out of the real
    /// project state.
    fn guard() -> std::sync::MutexGuard<'static, ()> {
        static G: OnceLock<Mutex<()>> = OnceLock::new();
        let dir = std::env::temp_dir().join("thoth-undo-tests/checkpoints");
        std::fs::create_dir_all(&dir).unwrap();
        let _ = dir_override().set(dir.clone());
        arm();
        let g = G.get_or_init(|| Mutex::new(()));
        let held = g.lock().unwrap_or_else(|e| e.into_inner());
        // each test starts from nothing, including checkpoints an earlier
        // run of the suite left behind
        for f in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let _ = std::fs::remove_file(f.path());
        }
        forget_pending();
        held
    }

    /// A change as a tool would have recorded it, at the moment it is made.
    fn change(path: &Path, before: Before) -> Change {
        Change {
            path: path.to_path_buf(),
            before,
            after: hash_of_file(path),
        }
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("thoth-undo-tests");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn a_snapshot_says_what_was_there() {
        let _g = guard();
        let p = tmp("snap.txt");
        std::fs::write(&p, "hello").unwrap();
        assert_eq!(snapshot(&p), Before::Text("hello".into()));
        std::fs::remove_file(&p).unwrap();
        assert_eq!(snapshot(&p), Before::Missing);
    }

    /// The condition that keeps undo from being a second way to lose work.
    #[test]
    fn a_file_touched_after_the_change_is_left_alone() {
        let _g = guard();
        let p = tmp("edited.txt");
        std::fs::write(&p, "thoth wrote this").unwrap();
        let c = Change {
            path: p.clone(),
            before: Before::Text("original".into()),
            after: Some(hash("thoth wrote this")),
        };
        // as thoth left it: restoring is fine
        assert_eq!(hash_of_file(&p), c.after);
        // the user has since changed it: the hash no longer matches
        std::fs::write(&p, "and then the user edited it").unwrap();
        assert_ne!(hash_of_file(&p), c.after);
    }

    /// The whole path: change a file, commit the request, undo it.
    #[test]
    fn a_committed_change_can_be_put_back() {
        let _g = guard();
        let p = tmp("roundtrip.txt");
        std::fs::write(&p, "first\n").unwrap();
        let before = snapshot(&p);
        std::fs::write(&p, "second\n").unwrap();
        assert_eq!(
            write_checkpoint("change the file", vec![change(&p, before)]),
            1
        );

        let r = undo_last().unwrap();
        assert_eq!(r.label, "change the file");
        assert!(
            r.restored.iter().any(|f| f.contains("roundtrip.txt")),
            "{r:?}",
        );
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "first\n");
        // the checkpoint is spent: still listed, but nothing left to undo
        assert!(
            list()
                .iter()
                .any(|l| l.contains("change the file") && l.contains("undone")),
            "{:?}",
            list()
        );
        assert!(undo_last().is_err());
    }

    /// A file created by thoth goes away again; the point is that undo does
    /// not leave half of a request behind.
    #[test]
    fn a_file_that_was_created_is_removed_again() {
        let _g = guard();
        let p = tmp("created.txt");
        let _ = std::fs::remove_file(&p);
        let before = snapshot(&p);
        assert_eq!(before, Before::Missing);
        std::fs::write(&p, "new\n").unwrap();
        write_checkpoint("write a new file", vec![change(&p, before)]);

        undo_last().unwrap();
        assert!(!p.exists(), "the created file should be gone");
    }

    #[test]
    fn a_file_changed_after_the_fact_is_reported_not_clobbered() {
        let _g = guard();
        let p = tmp("touched.txt");
        std::fs::write(&p, "original\n").unwrap();
        let before = snapshot(&p);
        std::fs::write(&p, "thoth wrote this\n").unwrap();
        write_checkpoint("edit it", vec![change(&p, before)]);

        std::fs::write(&p, "the user then edited it\n").unwrap();
        let r = undo_last().unwrap();
        assert!(r.restored.is_empty(), "{:?}", r.restored);
        assert_eq!(r.skipped.len(), 1);
        assert!(r.skipped[0].contains("changed since"), "{:?}", r.skipped);
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "the user then edited it\n",
            "someone else's edit is not ours to discard"
        );
    }

    /// Undo walks back a request at a time, and a spent checkpoint is kept:
    /// for a file that was left alone, the copy in it is the only one.
    #[test]
    fn undo_walks_back_and_keeps_what_it_could_not_restore() {
        let _g = guard();
        let p = tmp("history.txt");
        std::fs::write(&p, "one\n").unwrap();
        let first = snapshot(&p);
        std::fs::write(&p, "two\n").unwrap();
        write_checkpoint("first request", vec![change(&p, first)]);
        let second = snapshot(&p);
        std::fs::write(&p, "three\n").unwrap();
        write_checkpoint("second request", vec![change(&p, second)]);
        // the user edits it themselves, so the newest checkpoint cannot apply
        std::fs::write(&p, "mine\n").unwrap();

        let r = undo_last().unwrap();
        assert_eq!(r.label, "second request");
        assert!(r.restored.is_empty(), "{r:?}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "mine\n");
        // what that request would have put back is still on disk
        let kept = checkpoint_files()
            .iter()
            .filter_map(|f| std::fs::read_to_string(f).ok())
            .any(|t| t.contains("second request") && t.contains("two"));
        assert!(kept, "the spent checkpoint was thrown away");
        // and the next undo is the request before it, not the same one again
        assert_eq!(undo_last().unwrap().label, "first request");
        assert!(undo_last().is_err(), "nothing should be left");
    }

    /// A request too big to snapshot must still be reported as changed
    /// rather than dropped out of the checkpoint without a word.
    #[test]
    fn a_file_too_big_to_snapshot_is_still_named() {
        let _g = guard();
        let p = tmp("huge.txt");
        std::fs::write(&p, "current\n").unwrap();
        record(&p, Before::Text("x".repeat(MAX_SNAPSHOT_BYTES + 1)));
        // only what this test recorded: the fs tests run beside it and push
        // to the same pending list
        let mine: Vec<Change> = std::mem::take(&mut *pending().lock().unwrap())
            .into_iter()
            .filter(|c| c.path == p)
            .collect();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].before, Before::TooLarge);
        assert_eq!(write_checkpoint("write something enormous", mine), 1);

        let r = undo_last().unwrap();
        assert!(r.restored.is_empty(), "{r:?}");
        assert_eq!(r.skipped.len(), 1);
        assert!(r.skipped[0].contains("too large"), "{:?}", r.skipped);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "current\n");
    }

    #[test]
    fn a_request_that_changed_nothing_leaves_no_checkpoint() {
        let _g = guard();
        assert_eq!(write_checkpoint("just a question", Vec::new()), 0);
    }

    #[test]
    fn a_snapshot_that_would_be_enormous_is_not_taken() {
        let half = |n: usize| Before::Text("x".repeat(MAX_SNAPSHOT_BYTES / n));
        let taken = vec![
            change(Path::new("a"), half(2)),
            change(Path::new("b"), half(2)),
        ];
        // the budget is spent, so a big one is refused
        assert!(!fits(&taken, &Before::Text("y".repeat(1000))));
        // but a file that did not exist costs nothing to remember
        assert!(fits(&taken, &Before::Missing));
        assert!(fits(&[], &half(1)));
        assert!(!fits(
            &[],
            &Before::Text("y".repeat(MAX_SNAPSHOT_BYTES + 1))
        ));
    }
}
