use super::fs::{rel_slash_path, resolve, walk};
use anyhow::{Context, Result};
use regex::Regex;
use serde::Deserialize;
use std::path::Path;

const MAX_MATCHES: usize = 100;
const MAX_FILE_BYTES: u64 = 2_000_000;
const MAX_MATCH_CHARS: usize = 250;

#[derive(Deserialize)]
pub struct GrepArgs {
    pub pattern: String,
    pub path: Option<String>,
    pub glob: Option<String>,
}

pub fn grep(a: GrepArgs) -> Result<String> {
    let re = Regex::new(&a.pattern).with_context(|| format!("invalid regex: {}", a.pattern))?;
    let root = resolve(a.path.as_deref().unwrap_or("."));
    let glob_matcher = match &a.glob {
        Some(g) => Some(
            globset::GlobBuilder::new(g)
                .literal_separator(false)
                .build()
                .with_context(|| format!("invalid glob: {g}"))?
                .compile_matcher(),
        ),
        None => None,
    };

    let mut out = String::new();
    let mut matches = 0usize;
    let mut search_one = |path: &Path, rel: &str| -> bool {
        if let Some(m) = &glob_matcher
            && !m.is_match(rel)
            && !m.is_match(
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .as_ref(),
            )
        {
            return true;
        }
        let Ok(bytes) = std::fs::read(path) else {
            return true;
        };
        if bytes[..bytes.len().min(8192)].contains(&0) {
            return true; // binary
        }
        let text = String::from_utf8_lossy(&bytes);
        for (i, line) in text.lines().enumerate() {
            if re.is_match(line) {
                let line = line.trim();
                let line: String = line.chars().take(MAX_MATCH_CHARS).collect();
                out.push_str(&format!("{rel}:{}: {line}\n", i + 1));
                matches += 1;
                if matches >= MAX_MATCHES {
                    return false;
                }
            }
        }
        true
    };

    if root.is_file() {
        let rel = root.to_string_lossy().replace('\\', "/");
        search_one(&root, &rel);
    } else {
        for entry in walk(&root) {
            if entry
                .metadata()
                .map(|m| m.len() > MAX_FILE_BYTES)
                .unwrap_or(true)
            {
                continue;
            }
            let rel = rel_slash_path(entry.path(), &root);
            if !search_one(entry.path(), &rel) {
                break;
            }
        }
    }

    if out.is_empty() {
        return Ok("No matches found.".into());
    }
    if matches >= MAX_MATCHES {
        out.push_str(&format!("… (stopped at {MAX_MATCHES} matches)\n"));
    }
    Ok(out)
}
