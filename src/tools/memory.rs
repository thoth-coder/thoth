//! Persistent project memory, mirroring Claude Code's storage layout:
//! - `.thoth/memory.md` (in the project) — durable facts saved with the
//!   `remember` tool; shareable/committable, like CLAUDE.md.
//! - `~/.thoth/projects/<encoded-path>/last-session.md` (in the home dir) —
//!   recap written on every compact; private session data kept out of the
//!   repo, like ~/.claude/projects. Restored with /recap.

use anyhow::{Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

const MAX_MEMORY_CHARS: usize = 3000;

fn thoth_dir() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".thoth")
}

fn memory_path() -> PathBuf {
    thoth_dir().join("memory.md")
}

/// Encodes the cwd the way Claude Code does ("C:\Users\U\proj" ->
/// "C--Users-U-proj"), plus a hash of the real path. Without the hash,
/// `proj-a`, `proj_a` and `proj.a` would share one directory, and with it a
/// project's saved session and permissions cannot leak into another.
fn project_key() -> String {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let raw = cwd.to_string_lossy();
    // Windows paths are case-insensitive, so C:\Proj and c:\proj are one project
    let canonical = if cfg!(windows) {
        raw.to_lowercase()
    } else {
        raw.to_string()
    };
    let digest = Sha256::digest(canonical.as_bytes());
    let short: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    let readable: String = raw
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    format!("{readable}-{short}")
}

/// `~/.thoth/projects/<encoded-cwd>/` — private per-project state (recap,
/// saved session, permission allowlist). Never inside the repo.
pub fn project_dir() -> PathBuf {
    crate::config::thoth_home()
        .join("projects")
        .join(project_key())
}

fn recap_path() -> PathBuf {
    project_dir().join("last-session.md")
}

#[derive(Deserialize)]
pub struct RememberArgs {
    pub fact: String,
}

pub fn remember(a: RememberArgs) -> Result<String> {
    let fact = a.fact.trim();
    if fact.is_empty() {
        anyhow::bail!("fact is empty");
    }
    let path = memory_path();
    std::fs::create_dir_all(thoth_dir()).context("cannot create .thoth directory")?;
    let mut content = std::fs::read_to_string(&path).unwrap_or_default();
    if content.contains(fact) {
        return Ok("Already remembered.".into());
    }
    content.push_str(&format!("- {fact}\n"));
    std::fs::write(&path, content).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(format!("Remembered: {fact}"))
}

/// Memory contents for the system prompt (newest entries win the size cap).
pub fn load_memory() -> Option<String> {
    let text = std::fs::read_to_string(memory_path()).ok()?;
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if text.chars().count() <= MAX_MEMORY_CHARS {
        return Some(text.to_string());
    }
    // keep the newest lines that fit
    let mut kept: Vec<&str> = Vec::new();
    let mut size = 0usize;
    for line in text.lines().rev() {
        size += line.chars().count() + 1;
        if size > MAX_MEMORY_CHARS {
            break;
        }
        kept.push(line);
    }
    kept.reverse();
    Some(kept.join("\n"))
}

pub fn clear_memory() -> Result<()> {
    match std::fs::remove_file(memory_path()) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub fn save_recap(summary: &str) {
    let path = recap_path();
    if let Some(parent) = path.parent()
        && std::fs::create_dir_all(parent).is_ok()
    {
        let _ = std::fs::write(&path, summary);
    }
}

pub fn load_recap() -> Option<String> {
    // fall back to older locations: the key before it carried a path hash,
    // and before that the recap lived in the project itself
    let legacy_key: String = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    let legacy = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".thoth")
        .join("projects")
        .join(legacy_key)
        .join("last-session.md");
    let text = std::fs::read_to_string(recap_path())
        .or_else(|_| std::fs::read_to_string(legacy))
        .or_else(|_| std::fs::read_to_string(thoth_dir().join("last-session.md")))
        .ok()?;
    let text = text.trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}
