use super::fs::{rel_slash_path, resolve, walk};
use anyhow::{Context, Result};
use regex::Regex;
use serde::Deserialize;
use std::path::Path;

const MAX_MATCHES: usize = 100;
const MAX_FILE_BYTES: u64 = 2_000_000;
const MAX_MATCH_CHARS: usize = 250;
const MAX_CONTEXT: usize = 10;
// Stop early so the global tool-output cap does not cut a file mid-block.
/// Room left for the note at the end, which is the part that tells the
/// model what to do about a truncated search.
const OUT_FOOTER: usize = 120;

#[derive(Deserialize)]
pub struct GrepArgs {
    pub pattern: String,
    pub path: Option<String>,
    pub glob: Option<String>,
    pub context: Option<usize>,
}

/// `cap` is the caller's budget for the whole result. Stopping here rather
/// than letting the caller cut the text keeps the closing note, which is the
/// only part that says the search was incomplete.
pub fn grep(a: GrepArgs, cap: usize) -> Result<String> {
    let out_chars = cap.saturating_sub(OUT_FOOTER);
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

    let context = a.context.unwrap_or(0).min(MAX_CONTEXT);

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
        if context == 0 {
            for (i, line) in text.lines().enumerate() {
                if re.is_match(line) {
                    let line = line.trim();
                    let line: String = line.chars().take(MAX_MATCH_CHARS).collect();
                    out.push_str(&format!("{rel}:{}: {line}\n", i + 1));
                    matches += 1;
                    if matches >= MAX_MATCHES || out.chars().count() >= out_chars {
                        return false;
                    }
                }
            }
            return true;
        }

        let lines: Vec<&str> = text.lines().collect();
        let hits: Vec<usize> = (0..lines.len())
            .filter(|&i| re.is_match(lines[i]))
            .collect();
        if hits.is_empty() {
            return true;
        }
        // Merge overlapping context windows into blocks, ripgrep style.
        let mut blocks: Vec<(usize, usize)> = Vec::new();
        for &i in &hits {
            let start = i.saturating_sub(context);
            let end = (i + context).min(lines.len() - 1);
            match blocks.last_mut() {
                Some((_, e)) if start <= *e + 1 => *e = (*e).max(end),
                _ => blocks.push((start, end)),
            }
        }
        let hit_set: std::collections::HashSet<usize> = hits.iter().copied().collect();
        for (bi, (start, end)) in blocks.iter().enumerate() {
            if bi > 0 {
                out.push_str("--\n");
            }
            for (i, line) in lines.iter().enumerate().take(*end + 1).skip(*start) {
                let sep = if hit_set.contains(&i) { ':' } else { '-' };
                let line: String = line.chars().take(MAX_MATCH_CHARS).collect();
                out.push_str(&format!("{rel}:{}{sep} {line}\n", i + 1));
                if hit_set.contains(&i) {
                    matches += 1;
                }
                // check inside the loop: a file where everything matches
                // would otherwise build megabytes before anyone looks
                if matches >= MAX_MATCHES || out.chars().count() >= out_chars {
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
    } else if out.chars().count() >= out_chars {
        out.push_str("… (stopped, output limit reached; narrow the pattern or path)\n");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_temp(name: &str, content: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("thoth-grep-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, content).unwrap();
        p
    }

    fn run(pattern: &str, path: &Path, context: Option<usize>) -> String {
        grep(
            GrepArgs {
                pattern: pattern.into(),
                path: Some(path.to_string_lossy().into_owned()),
                glob: None,
                context,
            },
            12_000,
        )
        .unwrap()
    }

    #[test]
    fn context_shows_surrounding_lines() {
        let p = write_temp(
            "ctx.txt",
            "one\ntwo\nthree NEEDLE\nfour\nfive\nsix\nseven\neight\nnine NEEDLE\nten\n",
        );
        let out = run("NEEDLE", &p, Some(2));
        // matches marked with ':', context lines with '-'
        assert!(out.contains(":3: three NEEDLE"));
        assert!(out.contains(":1- one"));
        assert!(out.contains(":5- five"));
        assert!(out.contains(":9: nine NEEDLE"));
        // line 6 sits between the two windows and is not shown
        assert!(!out.contains("six"));
        // non-adjacent blocks are separated
        assert!(out.contains("--\n"));
    }

    #[test]
    fn overlapping_windows_merge_into_one_block() {
        let p = write_temp("merge.txt", "a\nNEEDLE\nb\nNEEDLE\nc\n");
        let out = run("NEEDLE", &p, Some(2));
        assert!(!out.contains("--\n"));
        assert!(out.contains(":1- a"));
        assert!(out.contains(":5- c"));
    }

    #[test]
    fn no_context_keeps_single_line_format() {
        let p = write_temp("plain.txt", "  padded NEEDLE\n");
        let out = run("NEEDLE", &p, None);
        assert!(out.contains(":1: padded NEEDLE"));
        assert!(!out.contains("--"));
    }
}
